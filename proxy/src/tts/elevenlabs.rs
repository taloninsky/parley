//! ElevenLabs HTTP `/stream` implementation of [`TtsProvider`].
//!
//! Spec: `docs/conversation-voice-slice-spec.md` §6.
//!
//! ## Endpoint shape
//!
//! ```text
//! POST https://api.elevenlabs.io/v1/text-to-speech/{voice_id}/stream
//!     ?output_format=mp3_44100_128
//! Headers: xi-api-key: <KEY>, content-type: application/json
//! Body: {"text": "...", "model_id": "eleven_multilingual_v2",
//!        "voice_settings": {"stability": 0.5, "similarity_boost": 0.75}}
//! ```
//!
//! Response is a streaming `audio/mpeg` body. Each bytes chunk is
//! forwarded verbatim to the orchestrator's broadcast fan-out and to
//! the on-disk cache; ElevenLabs takes care of MP3 frame alignment so
//! we don't need to buffer.

use std::sync::Arc;

use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use parley_core::chat::Cost;
use serde_json::json;

use super::{
    AudioFormat, SynthesisContext, TtsChunk, TtsError, TtsProvider, TtsRequest, TtsStream,
};

/// Default ElevenLabs streaming endpoint base. Constructor takes an
/// override so tests can point at a mock.
pub const ELEVENLABS_BASE_URL: &str = "https://api.elevenlabs.io/v1/text-to-speech";

/// Output format encoded into the query string. We pin 128 kbps MP3
/// per the spec — generous quality, trivial cost.
pub const ELEVENLABS_OUTPUT_FORMAT: &str = "mp3_44100_128";

/// Model id passed in the JSON body. Pinned to `eleven_multilingual_v2`:
/// the highest-quality pre-v3 model. Unlike `eleven_v3` it accepts
/// `previous_text` for cross-chunk prosody continuity (which is why
/// we picked it over v3); it does not support v3's bracketed
/// expressive tags (`[whisper]`, `[laugh]`, etc.). Per-character
/// price matches v3 ($0.000015 on the Creator tier), so
/// [`ELEVENLABS_COST_PER_CHAR_USD`] stays the same.
pub const ELEVENLABS_MODEL_ID: &str = "eleven_multilingual_v2";

/// Per-character USD cost on the Creator tier. Spec §6.
pub const ELEVENLABS_COST_PER_CHAR_USD: f64 = 0.000_015;

/// Concrete ElevenLabs implementation of [`TtsProvider`]. One
/// instance per (key, endpoint) pair; cheap to clone.
#[derive(Clone)]
pub struct ElevenLabsTts {
    api_key: Arc<str>,
    endpoint_base: Arc<str>,
    client: reqwest::Client,
}

impl ElevenLabsTts {
    /// Build a new provider with the production endpoint.
    pub fn new(api_key: impl Into<String>, client: reqwest::Client) -> Self {
        Self::with_endpoint(api_key, ELEVENLABS_BASE_URL, client)
    }

    /// Build a provider with a custom endpoint base — used by tests
    /// that mock the HTTP layer.
    pub fn with_endpoint(
        api_key: impl Into<String>,
        endpoint_base: impl Into<String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            api_key: api_key.into().into(),
            endpoint_base: endpoint_base.into().into(),
            client,
        }
    }

    /// Build the full URL for `voice_id`. Pure string composition.
    fn url_for(&self, voice_id: &str) -> String {
        format!(
            "{}/{}/stream?output_format={}",
            self.endpoint_base, voice_id, ELEVENLABS_OUTPUT_FORMAT
        )
    }
}

/// Whether the given ElevenLabs model id accepts the `previous_text`
/// (and `next_text`) request fields.
///
/// As of 2026-04 the v3 model returns HTTP 400 with
/// `"unsupported_model"` when these fields are present, while the
/// pre-v3 models (`eleven_turbo_v2_5`, `eleven_multilingual_v2`,
/// `eleven_monolingual_v1`) accept them. We therefore allow-list the
/// pre-v3 family and deny everything else by default; if ElevenLabs
/// adds support to v3 later, flipping this to a deny-list of just
/// `eleven_v3` is a one-line change.
fn model_supports_previous_text(model_id: &str) -> bool {
    matches!(
        model_id,
        "eleven_turbo_v2_5"
            | "eleven_turbo_v2"
            | "eleven_multilingual_v2"
            | "eleven_monolingual_v1"
    )
}

#[async_trait]
impl TtsProvider for ElevenLabsTts {
    fn id(&self) -> &'static str {
        "elevenlabs"
    }

    async fn synthesize(
        &self,
        request: TtsRequest,
        ctx: SynthesisContext,
    ) -> Result<TtsStream, TtsError> {
        // `ctx.previous_text` is forwarded as ElevenLabs'
        // `previous_text` field so the model picks prosody
        // appropriate to continuing speech rather than treating each
        // chunk as a fresh utterance. Without this, paragraph-leading
        // short words like "In", "So", "But" get read with an
        // exaggerated sentence-opener intonation and a small leading
        // pause.
        //
        // ElevenLabs' v3 model rejects this field with HTTP 400; the
        // pre-v3 family (multilingual v2, turbo v2.5, monolingual v1)
        // accepts it. We therefore allow-list those models via
        // [`model_supports_previous_text`] and silently omit the
        // field for any model not on the list. Our default
        // ([`ELEVENLABS_MODEL_ID`]) is `eleven_multilingual_v2`,
        // which is on the allow-list.
        //
        // `provider_state` (v2 request-id stitching) is still
        // unused; a future v2 adapter would read it here.
        if request.text.is_empty() {
            return Err(TtsError::Other("empty text".into()));
        }
        let characters = request.text.chars().count() as u32;

        let url = self.url_for(&request.voice_id);
        let mut body = json!({
            "text": request.text,
            "model_id": ELEVENLABS_MODEL_ID,
            "voice_settings": {
                "stability": 0.5,
                "similarity_boost": 0.75,
            }
        });
        if model_supports_previous_text(ELEVENLABS_MODEL_ID)
            && let Some(prev) = ctx.previous_text.as_deref()
            && !prev.is_empty()
        {
            // `as_object_mut().unwrap()` is safe: we just built `body`
            // as an object literal above.
            body.as_object_mut()
                .unwrap()
                .insert("previous_text".to_string(), json!(prev));
        }

        let resp = self
            .client
            .post(&url)
            .header("xi-api-key", self.api_key.as_ref())
            .header("content-type", "application/json")
            .header("accept", "audio/mpeg")
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
        Cost::from_usd(characters as f64 * ELEVENLABS_COST_PER_CHAR_USD)
    }

    fn output_format(&self) -> AudioFormat {
        // Pinned to the value sent on the query string above.
        AudioFormat::Mp3_44100_128
    }

    fn supports_expressive_tags(&self) -> bool {
        // The pre-v3 family (our default `eleven_multilingual_v2`
        // included) does not interpret v3's bracketed expressive
        // tag set (`[whisper]`, `[laugh]`, etc.) — those tags would
        // be read literally as text. The annotator pass uses this
        // flag to gate tag injection, so it stays disabled until we
        // (optionally) move back to a v3-family model.
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn provider(server: &MockServer) -> ElevenLabsTts {
        ElevenLabsTts::with_endpoint(
            "test-key",
            format!("{}/v1/text-to-speech", server.uri()),
            reqwest::Client::new(),
        )
    }

    #[tokio::test]
    async fn synthesize_emits_audio_then_done() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/text-to-speech/voice-1/stream"))
            .and(query_param("output_format", "mp3_44100_128"))
            .and(header("xi-api-key", "test-key"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "audio/mpeg")
                    .set_body_bytes(b"\xff\xfb\x90\x00fake-mp3-bytes" as &[u8]),
            )
            .mount(&server)
            .await;

        let p = provider(&server);
        let mut stream = p
            .synthesize(
                TtsRequest {
                    voice_id: "voice-1".into(),
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
    async fn http_error_surfaces_status_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/text-to-speech/voice-1/stream"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;

        let result = provider(&server)
            .synthesize(
                TtsRequest {
                    voice_id: "voice-1".into(),
                    text: "hi".into(),
                },
                SynthesisContext::default(),
            )
            .await;
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error, got Ok"),
        };
        match err {
            TtsError::Http { status, body } => {
                assert_eq!(status, 401);
                assert_eq!(body, "bad key");
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_text_is_rejected_locally() {
        let server = MockServer::start().await;
        let result = provider(&server)
            .synthesize(
                TtsRequest {
                    voice_id: "voice-1".into(),
                    text: String::new(),
                },
                SynthesisContext::default(),
            )
            .await;
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error, got Ok"),
        };
        assert!(matches!(err, TtsError::Other(_)));
    }

    #[test]
    fn cost_uses_creator_tier_rate() {
        let server_uri = "http://localhost:0/v1/text-to-speech".to_string();
        let p = ElevenLabsTts::with_endpoint("k", server_uri, reqwest::Client::new());
        // 1000 chars × $0.000015 = $0.015 — assert with float epsilon.
        let c = p.cost(1000);
        assert!((c.usd - 0.015).abs() < 1e-9, "got {}", c.usd);
    }

    #[test]
    fn id_returns_stable_string() {
        let p = ElevenLabsTts::new("k", reqwest::Client::new());
        assert_eq!(p.id(), "elevenlabs");
    }

    #[tokio::test]
    async fn synthesize_forwards_previous_text_on_default_model() {
        // Our pinned default (`eleven_multilingual_v2`) accepts
        // `previous_text`. When the orchestrator supplies context,
        // the request body must include the field.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/text-to-speech/voice-1/stream"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "audio/mpeg")
                    .set_body_bytes(b"\xff\xfb\x90\x00fake" as &[u8]),
            )
            .mount(&server)
            .await;

        let p = provider(&server);
        let mut stream = p
            .synthesize(
                TtsRequest {
                    voice_id: "voice-1".into(),
                    text: "Lean is great.".into(),
                },
                SynthesisContext {
                    previous_text: Some("Tactics are how you build proofs.".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        while let Some(item) = stream.next().await {
            item.unwrap();
        }

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let body = std::str::from_utf8(&received[0].body).unwrap();
        assert!(
            body.contains("\"previous_text\":\"Tactics are how you build proofs.\""),
            "default model request body must include previous_text: {body}",
        );
        // Sanity: the model id is the default we expect.
        assert!(body.contains("\"model_id\":\"eleven_multilingual_v2\""));
    }

    #[test]
    fn model_support_table_matches_known_eleven_models() {
        // Pre-v3 models accept previous_text.
        assert!(model_supports_previous_text("eleven_turbo_v2_5"));
        assert!(model_supports_previous_text("eleven_multilingual_v2"));
        assert!(model_supports_previous_text("eleven_monolingual_v1"));
        // Our pinned default is on the allow-list.
        assert!(model_supports_previous_text(ELEVENLABS_MODEL_ID));
        // v3 is not.
        assert!(!model_supports_previous_text("eleven_v3"));
        // Unknown ids deny by default — safer than guessing.
        assert!(!model_supports_previous_text("eleven_v4"));
    }

    #[tokio::test]
    async fn synthesize_omits_previous_text_when_absent_or_empty() {
        // With no `previous_text` (or an empty one), the request
        // body must NOT include the field. We capture the request
        // bodies and inspect them directly.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/text-to-speech/voice-1/stream"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "audio/mpeg")
                    .set_body_bytes(b"x" as &[u8]),
            )
            .mount(&server)
            .await;

        let p = provider(&server);
        // First call: previous_text = None.
        let mut s1 = p
            .synthesize(
                TtsRequest {
                    voice_id: "voice-1".into(),
                    text: "First.".into(),
                },
                SynthesisContext::default(),
            )
            .await
            .unwrap();
        while let Some(item) = s1.next().await {
            item.unwrap();
        }
        // Second call: previous_text = Some("") — also omitted.
        let mut s2 = p
            .synthesize(
                TtsRequest {
                    voice_id: "voice-1".into(),
                    text: "Second.".into(),
                },
                SynthesisContext {
                    previous_text: Some(String::new()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        while let Some(item) = s2.next().await {
            item.unwrap();
        }
        // Inspect the captured requests directly. Neither body
        // should mention `previous_text`.
        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 2);
        for req in received {
            let body = std::str::from_utf8(&req.body).unwrap();
            assert!(
                !body.contains("previous_text"),
                "request body unexpectedly carried previous_text: {body}",
            );
        }
    }
}
