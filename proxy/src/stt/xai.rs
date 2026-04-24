//! xAI `grok-stt` implementation of [`SttProvider`].
//!
//! The REST (batch / file) path is implemented here. The streaming
//! WebSocket path is stubbed as [`SttError::Unsupported`] and lands in
//! Step 3 WS — see `docs/xai-speech-integration-spec.md` §10.2.
//!
//! ## REST endpoint shape (§5.2)
//!
//! ```text
//! POST https://api.x.ai/v1/stt
//! Headers: Authorization: Bearer <KEY>
//! Multipart fields:
//!   model=grok-stt
//!   diarize=true|false
//!   language=<BCP-47>            (optional)
//!   audio_format=pcm             (only when file is raw PCM)
//!   sample_rate=<hz>             (only when file is raw PCM)
//!   file=<binary bytes>
//! ```
//!
//! Response is a JSON object with `{text, language, duration, words[]}`
//! at word granularity (no native utterance segmentation). We group
//! consecutive same-speaker words into [`TranscriptSegment`]s so the
//! canonical `Transcript.segments` matches what other providers emit.

use std::sync::Arc;

use async_trait::async_trait;
use parley_core::chat::Cost;
use parley_core::stt::{
    SttAudioFormat, SttRequest, SttStreamConfig, Transcript, TranscriptEvent, TranscriptSegment,
};
use serde::Deserialize;

use super::{SttError, SttProvider, SttResult, SttStreamHandle};
use crate::providers::ProviderId;

/// Production REST endpoint. Constructor takes an override so tests can
/// point at a `wiremock` server.
pub const XAI_STT_REST_URL: &str = "https://api.x.ai/v1/stt";

/// Production streaming WS endpoint. Wired up in Step 3 WS.
pub const XAI_STT_WS_URL: &str = "wss://api.x.ai/v1/stt";

/// Model id sent in the `model` multipart field. The only STT model xAI
/// ships today.
pub const XAI_STT_MODEL: &str = "grok-stt";

/// Batch / file tier — $0.10 per hour of input audio (spec §5.2 pricing
/// callout). Expressed per-second for ergonomic multiplication.
pub const XAI_STT_COST_BATCH_PER_SECOND: f64 = 0.10 / 3600.0;

/// Streaming tier — $0.20 per hour of input audio.
pub const XAI_STT_COST_STREAM_PER_SECOND: f64 = 0.20 / 3600.0;

/// Concrete xAI `grok-stt` provider. Cheap to clone — state is just an
/// `Arc<str>` key, a URL, and a `reqwest::Client`.
#[derive(Clone)]
pub struct XaiStt {
    api_key: Arc<str>,
    rest_url: Arc<str>,
    #[allow(dead_code)] // wired up in Step 3 WS
    ws_url: Arc<str>,
    client: reqwest::Client,
}

impl XaiStt {
    /// Build a provider pointed at the production endpoints.
    pub fn new(api_key: impl Into<String>, client: reqwest::Client) -> Self {
        Self::with_endpoints(api_key, XAI_STT_REST_URL, XAI_STT_WS_URL, client)
    }

    /// Build a provider with explicit endpoints — used by tests.
    pub fn with_endpoints(
        api_key: impl Into<String>,
        rest_url: impl Into<String>,
        ws_url: impl Into<String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            api_key: api_key.into().into(),
            rest_url: rest_url.into().into(),
            ws_url: ws_url.into().into(),
            client,
        }
    }
}

/// xAI REST word entry (§5.2 response body).
#[derive(Debug, Deserialize)]
struct WireWord {
    text: String,
    start: f64,
    end: f64,
    #[serde(default)]
    #[allow(dead_code)]
    confidence: Option<f64>,
    #[serde(default)]
    speaker: Option<serde_json::Value>,
}

/// xAI REST response shape.
#[derive(Debug, Deserialize)]
struct WireResponse {
    text: String,
    #[serde(default)]
    language: Option<String>,
    duration: f64,
    #[serde(default)]
    words: Vec<WireWord>,
}

/// File metadata attached to the multipart `file` part.
struct FilePart {
    filename: &'static str,
    mime: &'static str,
    audio_format_hint: Option<&'static str>,
    sample_rate: Option<u32>,
}

fn file_part_for(format: SttAudioFormat) -> FilePart {
    match format {
        SttAudioFormat::Pcm16Le { sample_rate_hz } => FilePart {
            filename: "audio.pcm",
            mime: "application/octet-stream",
            audio_format_hint: Some("pcm"),
            sample_rate: Some(sample_rate_hz),
        },
        SttAudioFormat::Wav => FilePart {
            filename: "audio.wav",
            mime: "audio/wav",
            audio_format_hint: None,
            sample_rate: None,
        },
        SttAudioFormat::Mp3 => FilePart {
            filename: "audio.mp3",
            mime: "audio/mpeg",
            audio_format_hint: None,
            sample_rate: None,
        },
        SttAudioFormat::Opus => FilePart {
            filename: "audio.opus",
            mime: "audio/opus",
            audio_format_hint: None,
            sample_rate: None,
        },
        SttAudioFormat::Flac => FilePart {
            filename: "audio.flac",
            mime: "audio/flac",
            audio_format_hint: None,
            sample_rate: None,
        },
    }
}

/// Normalize xAI's speaker value (REST returns integers; the WS variant
/// has been observed emitting strings) into a canonical `"0"`, `"1"`, …
/// label so downstream segments compare cleanly regardless of source.
fn speaker_label(raw: &Option<serde_json::Value>) -> Option<String> {
    match raw {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        Some(other) => Some(other.to_string()),
    }
}

/// Group consecutive same-speaker words into [`TranscriptSegment`]s.
/// xAI returns word-level output; our canonical `Transcript.segments` is
/// utterance-level, so the first cut is "one segment per contiguous run
/// of words sharing a speaker id" — a reasonable approximation when we
/// don't have punctuation-aware boundaries.
fn words_to_segments(words: &[WireWord]) -> Vec<TranscriptSegment> {
    let mut out: Vec<TranscriptSegment> = Vec::new();
    for w in words {
        let speaker = speaker_label(&w.speaker);
        let last_speaker = out.last().and_then(|s| s.speaker.clone());
        if out.is_empty() || last_speaker != speaker {
            out.push(TranscriptSegment {
                text: w.text.clone(),
                start_seconds: w.start,
                end_seconds: w.end,
                speaker,
            });
        } else if let Some(seg) = out.last_mut() {
            if !seg.text.is_empty() {
                seg.text.push(' ');
            }
            seg.text.push_str(&w.text);
            seg.end_seconds = w.end;
        }
    }
    out
}

#[async_trait]
impl SttProvider for XaiStt {
    fn id(&self) -> ProviderId {
        ProviderId::Xai
    }

    async fn transcribe(&self, request: SttRequest) -> SttResult<Transcript> {
        if request.audio.is_empty() {
            return Err(SttError::Other("empty audio payload".into()));
        }

        let part_meta = file_part_for(request.format);
        let file_part = reqwest::multipart::Part::bytes(request.audio)
            .file_name(part_meta.filename)
            .mime_str(part_meta.mime)
            .map_err(|e| SttError::Other(format!("bad mime: {e}")))?;

        let mut form = reqwest::multipart::Form::new()
            .text("model", XAI_STT_MODEL)
            .text("diarize", if request.diarize { "true" } else { "false" })
            .part("file", file_part);

        if let Some(lang) = request.language.as_deref() {
            form = form.text("language", lang.to_string());
        }
        if let Some(fmt) = part_meta.audio_format_hint {
            form = form.text("audio_format", fmt);
        }
        if let Some(sr) = part_meta.sample_rate {
            form = form.text("sample_rate", sr.to_string());
        }

        let resp = self
            .client
            .post(self.rest_url.as_ref())
            .bearer_auth(self.api_key.as_ref())
            .multipart(form)
            .send()
            .await
            .map_err(|e| SttError::Transport(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return Err(SttError::Auth(body));
            }
            return Err(SttError::Http {
                status: status.as_u16(),
                body,
            });
        }

        let wire: WireResponse = resp
            .json()
            .await
            .map_err(|e| SttError::BadResponse(e.to_string()))?;

        let segments = words_to_segments(&wire.words);
        Ok(Transcript {
            text: wire.text,
            segments,
            language: wire.language.filter(|s| !s.is_empty()),
            duration_seconds: wire.duration,
        })
    }

    async fn stream(&self, _config: SttStreamConfig) -> SttResult<SttStreamHandle> {
        // Lands in Step 3 WS — see `docs/xai-speech-integration-spec.md` §10.2.
        Err(SttError::Unsupported(
            "xAI STT streaming WS lands in Step 3 WS".into(),
        ))
    }

    fn cost(&self, seconds: f64, streaming: bool) -> Cost {
        let rate = if streaming {
            XAI_STT_COST_STREAM_PER_SECOND
        } else {
            XAI_STT_COST_BATCH_PER_SECOND
        };
        Cost::from_usd(seconds.max(0.0) * rate)
    }
}

// Silence the unused-import lint until `TranscriptEvent` is referenced
// by the WS client in Step 3 WS.
#[allow(dead_code)]
const _TRANSCRIPT_EVENT_PLACEHOLDER: Option<TranscriptEvent> = None;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn provider(server: &MockServer) -> XaiStt {
        XaiStt::with_endpoints(
            "test-key",
            format!("{}/v1/stt", server.uri()),
            "wss://unused.invalid/v1/stt",
            reqwest::Client::new(),
        )
    }

    fn sample_response() -> serde_json::Value {
        json!({
            "text": "hello world",
            "language": "en",
            "duration": 1.4,
            "words": [
                {"text": "hello", "start": 0.0, "end": 0.5, "confidence": 0.95, "speaker": 0},
                {"text": "world", "start": 0.6, "end": 1.2, "confidence": 0.98, "speaker": 0}
            ],
            "channels": []
        })
    }

    #[tokio::test]
    async fn transcribe_happy_path_returns_canonical_transcript() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/stt"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(sample_response()))
            .mount(&server)
            .await;

        let p = provider(&server);
        let t = p
            .transcribe(SttRequest {
                audio: b"RIFF....fake-wav".to_vec(),
                format: SttAudioFormat::Wav,
                language: Some("en".into()),
                diarize: true,
            })
            .await
            .unwrap();

        assert_eq!(t.text, "hello world");
        assert_eq!(t.language.as_deref(), Some("en"));
        assert!((t.duration_seconds - 1.4).abs() < 1e-9);
        // Two same-speaker words collapse into one segment.
        assert_eq!(t.segments.len(), 1);
        assert_eq!(t.segments[0].text, "hello world");
        assert_eq!(t.segments[0].speaker.as_deref(), Some("0"));
        assert!((t.segments[0].start_seconds - 0.0).abs() < 1e-9);
        assert!((t.segments[0].end_seconds - 1.2).abs() < 1e-9);
    }

    #[tokio::test]
    async fn transcribe_sends_expected_multipart_fields_for_pcm() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/stt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(sample_response()))
            .mount(&server)
            .await;

        let _ = provider(&server)
            .transcribe(SttRequest {
                audio: vec![0u8, 1, 2, 3, 4, 5, 6, 7],
                format: SttAudioFormat::Pcm16Le {
                    sample_rate_hz: 16000,
                },
                language: None,
                diarize: false,
            })
            .await
            .unwrap();

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let body = extract_multipart_parts(&received[0]);
        assert_eq!(body.get("model").map(String::as_str), Some("grok-stt"));
        assert_eq!(body.get("diarize").map(String::as_str), Some("false"));
        assert_eq!(body.get("audio_format").map(String::as_str), Some("pcm"));
        assert_eq!(body.get("sample_rate").map(String::as_str), Some("16000"));
        // language omitted when None.
        assert!(!body.contains_key("language"));
        // file part present.
        assert!(body.contains_key("file"));
    }

    #[tokio::test]
    async fn auth_failure_maps_to_auth_variant() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/stt"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;

        let err = provider(&server)
            .transcribe(SttRequest {
                audio: b"x".to_vec(),
                format: SttAudioFormat::Wav,
                language: None,
                diarize: true,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, SttError::Auth(ref b) if b == "bad key"), "got {err:?}");
    }

    #[tokio::test]
    async fn non_auth_http_error_maps_to_http_variant() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/stt"))
            .respond_with(ResponseTemplate::new(500).set_body_string("oops"))
            .mount(&server)
            .await;

        let err = provider(&server)
            .transcribe(SttRequest {
                audio: b"x".to_vec(),
                format: SttAudioFormat::Wav,
                language: None,
                diarize: true,
            })
            .await
            .unwrap_err();
        match err {
            SttError::Http { status, body } => {
                assert_eq!(status, 500);
                assert_eq!(body, "oops");
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_audio_is_rejected_locally() {
        let server = MockServer::start().await;
        let err = provider(&server)
            .transcribe(SttRequest {
                audio: vec![],
                format: SttAudioFormat::Wav,
                language: None,
                diarize: true,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, SttError::Other(_)), "got {err:?}");
        // The mock was never hit.
        let received = server.received_requests().await.unwrap();
        assert!(received.is_empty());
    }

    #[tokio::test]
    async fn stream_returns_unsupported_in_rest_only_step() {
        let p = XaiStt::new("k", reqwest::Client::new());
        let result = p
            .stream(SttStreamConfig {
                format: SttAudioFormat::Pcm16Le {
                    sample_rate_hz: 16000,
                },
                language: None,
                diarize: true,
            })
            .await;
        // `SttStreamHandle` isn't `Debug`, so match explicitly rather
        // than going through `unwrap_err()`.
        match result {
            Err(SttError::Unsupported(_)) => {}
            Err(other) => panic!("expected Unsupported, got {other:?}"),
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }

    #[test]
    fn cost_uses_batch_rate_by_default_and_stream_rate_on_request() {
        let p = XaiStt::new("k", reqwest::Client::new());
        // 1 hour batch = $0.10
        assert!((p.cost(3600.0, false).usd - 0.10).abs() < 1e-9);
        // 1 hour streaming = $0.20
        assert!((p.cost(3600.0, true).usd - 0.20).abs() < 1e-9);
        // Negative seconds clamps to zero.
        assert_eq!(p.cost(-5.0, false).usd, 0.0);
    }

    #[test]
    fn id_returns_xai() {
        let p = XaiStt::new("k", reqwest::Client::new());
        assert_eq!(p.id(), ProviderId::Xai);
    }

    #[test]
    fn words_to_segments_splits_on_speaker_change() {
        let words = vec![
            WireWord {
                text: "hi".into(),
                start: 0.0,
                end: 0.3,
                confidence: None,
                speaker: Some(json!(0)),
            },
            WireWord {
                text: "there".into(),
                start: 0.4,
                end: 0.7,
                confidence: None,
                speaker: Some(json!(0)),
            },
            WireWord {
                text: "hello".into(),
                start: 0.8,
                end: 1.2,
                confidence: None,
                speaker: Some(json!(1)),
            },
        ];
        let segs = words_to_segments(&words);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, "hi there");
        assert_eq!(segs[0].speaker.as_deref(), Some("0"));
        assert_eq!(segs[1].text, "hello");
        assert_eq!(segs[1].speaker.as_deref(), Some("1"));
    }

    /// Lightweight multipart body parser — just enough to pull field
    /// names → text values and notice the presence of the `file` part.
    /// Avoids pulling `multer` in for the sake of one test.
    fn extract_multipart_parts(req: &Request) -> std::collections::HashMap<String, String> {
        let ctype = req
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        let boundary = ctype
            .split(';')
            .filter_map(|p| p.trim().strip_prefix("boundary="))
            .next()
            .expect("multipart boundary in content-type");
        let delim = format!("--{boundary}");
        let body = std::str::from_utf8(&req.body).expect("multipart body is utf-8-ish");
        let mut out = std::collections::HashMap::new();
        for part in body.split(&delim) {
            let part = part.trim_start_matches("\r\n").trim_end_matches("\r\n");
            if part.is_empty() || part == "--" {
                continue;
            }
            let (headers, content) = match part.split_once("\r\n\r\n") {
                Some(pair) => pair,
                None => continue,
            };
            let name = headers
                .lines()
                .find_map(|h| {
                    let h = h.trim_end_matches('\r');
                    h.to_ascii_lowercase()
                        .starts_with("content-disposition:")
                        .then(|| h.to_string())
                })
                .and_then(|h| {
                    h.split(';').find_map(|p| {
                        let p = p.trim();
                        p.strip_prefix("name=")
                            .map(|v| v.trim_matches('"').to_string())
                    })
                });
            if let Some(n) = name {
                let value = content.trim_end_matches("\r\n").to_string();
                out.insert(n, value);
            }
        }
        out
    }
}
