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
//! Body: {"text": "...", "model_id": "eleven_v3",
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

/// Model id passed in the JSON body. Pinned to the v3 conversational
/// model.
pub const ELEVENLABS_MODEL_ID: &str = "eleven_v3";

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

#[async_trait]
impl TtsProvider for ElevenLabsTts {
    fn id(&self) -> &'static str {
        "elevenlabs"
    }

    async fn synthesize(
        &self,
        request: TtsRequest,
        _ctx: SynthesisContext,
    ) -> Result<TtsStream, TtsError> {
        // The v3 model handles cross-chunk prosody internally and
        // does not consume `previous_request_ids`/`previous_text`,
        // so `_ctx` is intentionally ignored. A future v2-stitching
        // adapter would read `ctx.provider_state` here.
        if request.text.is_empty() {
            return Err(TtsError::Other("empty text".into()));
        }
        let characters = request.text.chars().count() as u32;

        let url = self.url_for(&request.voice_id);
        let body = json!({
            "text": request.text,
            "model_id": ELEVENLABS_MODEL_ID,
            "voice_settings": {
                "stability": 0.5,
                "similarity_boost": 0.75,
            }
        });

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
        // ElevenLabs v3 understands the bracketed expressive tag set
        // (`[whisper]`, `[laugh]`, etc.). The annotator pass uses
        // this to gate tag injection.
        true
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
}
