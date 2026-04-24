//! xAI `grok-tts` implementation of [`TtsProvider`].
//!
//! REST (unary) path only — the streaming WebSocket path is a Step 4
//! WS follow-up (see `docs/xai-speech-integration-spec.md` §10.2).
//!
//! We pin the response codec to `Mp3_44100_128` so the existing
//! [`crate::tts::silence`] splicer keeps working unchanged across
//! providers. xAI supports other codecs and sample rates; surfacing
//! them waits until [`AudioFormat`] grows matching variants.
//!
//! ## Endpoint shape (§5.4)
//!
//! ```text
//! POST https://api.x.ai/v1/tts
//! Headers: Authorization: Bearer <KEY>, content-type: application/json
//! Body: {"text":"…","voice_id":"eve","language":"en",
//!        "output_format":{"codec":"mp3","sample_rate":44100,"bit_rate":128000},
//!        "text_normalization":true}
//! ```

use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use parley_core::chat::Cost;
use parley_core::tts::VoiceDescriptor;
use serde::Deserialize;
use serde_json::json;

use super::{
    AudioFormat, SynthesisContext, TtsChunk, TtsError, TtsProvider, TtsRequest, TtsStream,
};

/// Default REST endpoint. Constructor takes an override so tests can
/// point at a `wiremock` server.
pub const XAI_TTS_REST_URL: &str = "https://api.x.ai/v1/tts";

/// Default voices catalog endpoint (spec §5.6). 24-hour cache TTL
/// matches the server-side contract.
pub const XAI_TTS_VOICES_URL: &str = "https://api.x.ai/v1/tts/voices";

/// TTL for the voices-catalog cache. Spec §5.6 mandates 24h.
pub const XAI_TTS_VOICES_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Pinned MP3 sample rate in Hz — matches [`AudioFormat::Mp3_44100_128`]
/// so the silence splicer can crossfade without re-encoding.
pub const XAI_TTS_SAMPLE_RATE: u32 = 44100;

/// Pinned MP3 bit rate in bits-per-second — matches
/// [`AudioFormat::Mp3_44100_128`].
pub const XAI_TTS_BIT_RATE: u32 = 128_000;

/// Per-character USD cost — $4.20 / 1 M chars. Spec §5 / §5.4 pricing.
pub const XAI_TTS_COST_PER_CHAR_USD: f64 = 4.20 / 1_000_000.0;

/// Default voice. `eve` is the documented default in spec §5.4.
pub const XAI_TTS_DEFAULT_VOICE: &str = "eve";

/// The five canonical xAI voice IDs shipped today (spec §5.4). The
/// voices catalog endpoint (§5.6) can override this at runtime; this
/// list is the known-good baseline used when the catalog hasn't been
/// fetched yet.
pub const XAI_TTS_KNOWN_VOICES: &[&str] = &["eve", "ara", "rex", "sal", "leo"];

/// Concrete xAI implementation of [`TtsProvider`]. One instance per
/// (key, endpoint) pair; cheap to clone.
#[derive(Clone)]
pub struct XaiTts {
    api_key: Arc<str>,
    endpoint: Arc<str>,
    voices_endpoint: Arc<str>,
    client: reqwest::Client,
    voices_cache: Arc<Mutex<Option<VoicesCacheEntry>>>,
}

#[derive(Clone)]
struct VoicesCacheEntry {
    fetched_at: Instant,
    voices: Vec<VoiceDescriptor>,
}

impl XaiTts {
    /// Build a provider pointed at the production endpoints.
    pub fn new(api_key: impl Into<String>, client: reqwest::Client) -> Self {
        Self::with_endpoints(api_key, XAI_TTS_REST_URL, XAI_TTS_VOICES_URL, client)
    }

    /// Build a provider with a custom synthesize endpoint. Voices URL
    /// defaults to the production catalog — callers who want to stub
    /// both should use [`Self::with_endpoints`].
    pub fn with_endpoint(
        api_key: impl Into<String>,
        endpoint: impl Into<String>,
        client: reqwest::Client,
    ) -> Self {
        Self::with_endpoints(api_key, endpoint, XAI_TTS_VOICES_URL, client)
    }

    /// Build a provider with overrides for both the synthesize and
    /// voices-catalog endpoints — used by tests that stand up a
    /// `wiremock` server.
    pub fn with_endpoints(
        api_key: impl Into<String>,
        endpoint: impl Into<String>,
        voices_endpoint: impl Into<String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            api_key: api_key.into().into(),
            endpoint: endpoint.into().into(),
            voices_endpoint: voices_endpoint.into().into(),
            client,
            voices_cache: Arc::new(Mutex::new(None)),
        }
    }
}

#[async_trait]
impl TtsProvider for XaiTts {
    fn id(&self) -> &'static str {
        "xai"
    }

    async fn synthesize(
        &self,
        request: TtsRequest,
        _ctx: SynthesisContext,
    ) -> Result<TtsStream, TtsError> {
        if request.text.is_empty() {
            return Err(TtsError::Other("empty text".into()));
        }
        let characters = request.text.chars().count() as u32;

        // xAI accepts either a specific voice id or the string "eve" as
        // default. An empty voice_id from the caller means "use default"
        // per the spec's "default eve" note.
        let voice = if request.voice_id.is_empty() {
            XAI_TTS_DEFAULT_VOICE.to_string()
        } else {
            request.voice_id
        };

        let body = json!({
            "text": request.text,
            "voice_id": voice,
            "output_format": {
                "codec": "mp3",
                "sample_rate": XAI_TTS_SAMPLE_RATE,
                "bit_rate": XAI_TTS_BIT_RATE,
            },
            "text_normalization": true,
        });

        let resp = self
            .client
            .post(self.endpoint.as_ref())
            .bearer_auth(self.api_key.as_ref())
            .header("accept", "audio/mpeg")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| TtsError::Transport(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(TtsError::Http {
                status: status.as_u16(),
                body,
            });
        }

        let mut bytes = resp.bytes_stream();
        let stream = try_stream! {
            while let Some(next) = bytes.next().await {
                let chunk = next.map_err(|e| TtsError::Transport(e.to_string()))?;
                if !chunk.is_empty() {
                    yield TtsChunk::Audio(chunk.to_vec());
                }
            }
            yield TtsChunk::Done { characters };
        };
        Ok(Box::pin(stream))
    }

    fn cost(&self, characters: u32) -> Cost {
        Cost::from_usd(characters as f64 * XAI_TTS_COST_PER_CHAR_USD)
    }

    fn output_format(&self) -> AudioFormat {
        // Pinned to 44.1 kHz 128 kbps MP3 so the silence splicer's
        // pre-baked silence frame lines up.
        AudioFormat::Mp3_44100_128
    }

    fn supports_expressive_tags(&self) -> bool {
        // xAI's models don't interpret ElevenLabs-style bracketed
        // expressive tags; they'd be read literally.
        false
    }

    async fn voices(&self) -> Result<Vec<VoiceDescriptor>, TtsError> {
        if let Some(cached) = self.voices_from_cache() {
            return Ok(cached);
        }

        let resp = self
            .client
            .get(self.voices_endpoint.as_ref())
            .bearer_auth(self.api_key.as_ref())
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| TtsError::Transport(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(TtsError::Http {
                status: status.as_u16(),
                body,
            });
        }

        let body: VoicesResponse = resp
            .json()
            .await
            .map_err(|e| TtsError::Protocol(format!("voices parse: {e}")))?;
        let voices = body.into_descriptors();
        self.store_voices_cache(&voices);
        Ok(voices)
    }
}

impl XaiTts {
    fn voices_from_cache(&self) -> Option<Vec<VoiceDescriptor>> {
        let guard = self.voices_cache.lock().ok()?;
        let entry = guard.as_ref()?;
        if entry.fetched_at.elapsed() < XAI_TTS_VOICES_TTL {
            Some(entry.voices.clone())
        } else {
            None
        }
    }

    fn store_voices_cache(&self, voices: &[VoiceDescriptor]) {
        if let Ok(mut guard) = self.voices_cache.lock() {
            *guard = Some(VoicesCacheEntry {
                fetched_at: Instant::now(),
                voices: voices.to_vec(),
            });
        }
    }
}

/// xAI's documented voices response is `{ "voices": [ { "id": "eve",
/// ... }, ... ] }` (spec §5.6 — exact schema unstable; we only
/// depend on `id` plus optional `display_name`/`language_tags`).
#[derive(Debug, Deserialize)]
struct VoicesResponse {
    #[serde(default)]
    voices: Vec<VoiceEntry>,
}

#[derive(Debug, Deserialize)]
struct VoiceEntry {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    language_tags: Vec<String>,
    // Accepted for forward-compat even if we don't project them into
    // `VoiceDescriptor` today.
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
}

impl VoicesResponse {
    fn into_descriptors(self) -> Vec<VoiceDescriptor> {
        self.voices
            .into_iter()
            .map(|v| VoiceDescriptor {
                display_name: v
                    .display_name
                    .unwrap_or_else(|| title_case(&v.id)),
                id: v.id,
                language_tags: v.language_tags,
            })
            .collect()
    }
}

fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn provider(server: &MockServer) -> XaiTts {
        XaiTts::with_endpoint(
            "test-key",
            format!("{}/v1/tts", server.uri()),
            reqwest::Client::new(),
        )
    }

    #[tokio::test]
    async fn synthesize_emits_audio_then_done() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tts"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "audio/mpeg")
                    .set_body_bytes(b"\xff\xfb\x90\x00fake-mp3-bytes" as &[u8]),
            )
            .mount(&server)
            .await;

        let mut stream = provider(&server)
            .synthesize(
                TtsRequest {
                    voice_id: "eve".into(),
                    text: "hello world".into(),
                },
                SynthesisContext::default(),
            )
            .await
            .unwrap();

        let mut audio = Vec::new();
        let mut saw_done = false;
        while let Some(item) = stream.next().await {
            match item.unwrap() {
                TtsChunk::Audio(bytes) => audio.extend(bytes),
                TtsChunk::Done { characters } => {
                    assert_eq!(characters, 11);
                    saw_done = true;
                }
            }
        }
        assert!(saw_done);
        assert_eq!(audio, b"\xff\xfb\x90\x00fake-mp3-bytes");
    }

    #[tokio::test]
    async fn synthesize_sends_expected_json_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tts"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "audio/mpeg")
                    .set_body_bytes(b"x" as &[u8]),
            )
            .mount(&server)
            .await;

        let mut s = provider(&server)
            .synthesize(
                TtsRequest {
                    voice_id: "rex".into(),
                    text: "ok".into(),
                },
                SynthesisContext::default(),
            )
            .await
            .unwrap();
        while let Some(item) = s.next().await {
            item.unwrap();
        }

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let body = std::str::from_utf8(&received[0].body).unwrap();
        assert!(body.contains("\"text\":\"ok\""));
        assert!(body.contains("\"voice_id\":\"rex\""));
        assert!(body.contains("\"codec\":\"mp3\""));
        assert!(body.contains("\"sample_rate\":44100"));
        assert!(body.contains("\"bit_rate\":128000"));
        assert!(body.contains("\"text_normalization\":true"));
    }

    #[tokio::test]
    async fn empty_voice_id_falls_back_to_default() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tts"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "audio/mpeg")
                    .set_body_bytes(b"x" as &[u8]),
            )
            .mount(&server)
            .await;

        let mut s = provider(&server)
            .synthesize(
                TtsRequest {
                    voice_id: String::new(),
                    text: "hi".into(),
                },
                SynthesisContext::default(),
            )
            .await
            .unwrap();
        while let Some(item) = s.next().await {
            item.unwrap();
        }
        let received = server.received_requests().await.unwrap();
        let body = std::str::from_utf8(&received[0].body).unwrap();
        assert!(body.contains("\"voice_id\":\"eve\""));
    }

    #[tokio::test]
    async fn http_error_surfaces_status_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tts"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;

        let result = provider(&server)
            .synthesize(
                TtsRequest {
                    voice_id: "eve".into(),
                    text: "hi".into(),
                },
                SynthesisContext::default(),
            )
            .await;
        match result {
            Err(TtsError::Http { status, body }) => {
                assert_eq!(status, 401);
                assert_eq!(body, "bad key");
            }
            Err(other) => panic!("expected Http, got {other:?}"),
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }

    #[tokio::test]
    async fn empty_text_is_rejected_locally() {
        let server = MockServer::start().await;
        let result = provider(&server)
            .synthesize(
                TtsRequest {
                    voice_id: "eve".into(),
                    text: String::new(),
                },
                SynthesisContext::default(),
            )
            .await;
        match result {
            Err(TtsError::Other(_)) => {}
            Err(other) => panic!("expected Other, got {other:?}"),
            Ok(_) => panic!("expected Err, got Ok"),
        }
        // Mock was never hit.
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[test]
    fn cost_uses_spec_per_char_rate() {
        let p = XaiTts::new("k", reqwest::Client::new());
        // 1_000_000 chars × $4.20/1M = $4.20.
        let c = p.cost(1_000_000);
        assert!((c.usd - 4.20).abs() < 1e-9, "got {}", c.usd);
    }

    #[test]
    fn id_and_format_are_stable() {
        let p = XaiTts::new("k", reqwest::Client::new());
        assert_eq!(p.id(), "xai");
        assert_eq!(p.output_format(), AudioFormat::Mp3_44100_128);
        assert!(!p.supports_expressive_tags());
    }

    #[test]
    fn known_voices_contains_documented_five() {
        for id in ["eve", "ara", "rex", "sal", "leo"] {
            assert!(XAI_TTS_KNOWN_VOICES.contains(&id), "missing voice: {id}");
        }
        assert_eq!(XAI_TTS_KNOWN_VOICES.len(), 5);
    }

    // ── voices() catalog ────────────────────────────────────────────

    fn provider_with_voices(server: &MockServer) -> XaiTts {
        XaiTts::with_endpoints(
            "test-key",
            format!("{}/v1/tts", server.uri()),
            format!("{}/v1/tts/voices", server.uri()),
            reqwest::Client::new(),
        )
    }

    #[tokio::test]
    async fn voices_parses_catalog_response() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/tts/voices"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"voices":[
                    {"id":"eve","display_name":"Eve","language_tags":["en-US","es-MX"]},
                    {"id":"rex","language_tags":["en-US"]},
                    {"id":"sal"}
                ]}"#,
            ))
            .mount(&server)
            .await;

        let voices = provider_with_voices(&server).voices().await.unwrap();
        assert_eq!(voices.len(), 3);
        assert_eq!(voices[0].id, "eve");
        assert_eq!(voices[0].display_name, "Eve");
        assert_eq!(voices[0].language_tags, vec!["en-US", "es-MX"]);
        // Missing display_name falls back to title-cased id.
        assert_eq!(voices[1].id, "rex");
        assert_eq!(voices[1].display_name, "Rex");
        // Missing language_tags decodes as empty.
        assert_eq!(voices[2].id, "sal");
        assert_eq!(voices[2].display_name, "Sal");
        assert!(voices[2].language_tags.is_empty());
    }

    #[tokio::test]
    async fn voices_second_call_uses_cache() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/tts/voices"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"voices":[{"id":"eve"}]}"#,
            ))
            .expect(1) // second .voices() must not hit the mock
            .mount(&server)
            .await;

        let p = provider_with_voices(&server);
        let first = p.voices().await.unwrap();
        let second = p.voices().await.unwrap();
        assert_eq!(first, second);
        // `expect(1)` above is the real assertion — wiremock panics on
        // Drop if the mount count doesn't match.
    }

    #[tokio::test]
    async fn voices_surfaces_http_error_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/tts/voices"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;

        match provider_with_voices(&server).voices().await {
            Err(TtsError::Http { status, body }) => {
                assert_eq!(status, 401);
                assert_eq!(body, "bad key");
            }
            Err(other) => panic!("expected Http, got {other:?}"),
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }

    #[tokio::test]
    async fn voices_surfaces_protocol_error_on_bad_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/tts/voices"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string("not-json"),
            )
            .mount(&server)
            .await;

        match provider_with_voices(&server).voices().await {
            Err(TtsError::Protocol(msg)) => {
                assert!(msg.contains("voices parse"));
            }
            Err(other) => panic!("expected Protocol, got {other:?}"),
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }

    #[test]
    fn title_case_capitalizes_first_char_only() {
        assert_eq!(title_case("eve"), "Eve");
        assert_eq!(title_case(""), "");
        assert_eq!(title_case("e"), "E");
        assert_eq!(title_case("MiXeD"), "MiXeD");
    }
}
