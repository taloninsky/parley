//! Cartesia Sonic-3 implementation of [`TtsProvider`] over WebSocket.
//!
//! Spec: `docs/cartesia-sonic-3-integration-spec.md`.
//!
//! ## Why WebSocket on day one
//!
//! ElevenLabs and xAI both go through the per-request HTTP path
//! today, and we hear audible prosody breaks at chunk boundaries.
//! Cartesia's WebSocket protocol exposes a per-turn `context_id`
//! plus `continue: true`, which is the canonical fix. We open one
//! WebSocket per `synthesize()` call (matching the per-request trait
//! shape), and within a turn we reuse the `context_id` across chunks.
//!
//! ## Endpoint shape (§7.1)
//!
//! ```text
//! URL:  wss://api.cartesia.ai/tts/websocket
//!         ?api_key=<key>&cartesia_version=2026-03-01
//!
//! Send (single JSON text frame per chunk):
//! {
//!   "model_id": "sonic-3",
//!   "transcript": "<text>",
//!   "voice":  { "mode": "id", "id": "<voice_uuid>" },
//!   "language": "en",
//!   "context_id": "<uuid>",
//!   "continue": <bool>,
//!   "add_timestamps": false,
//!   "output_format": {
//!     "container": "raw",
//!     "encoding":  "pcm_s16le",
//!     "sample_rate": 44100
//!   }
//! }
//!
//! Receive (text frames only — Cartesia does NOT use binary WS frames):
//!   {"type":"chunk", "data":"<base64 pcm_s16le>", "done":false, ...}  -> Audio
//!   {"type":"timestamps",  ...}                                       -> discard
//!   {"type":"flush_done",  ...}                                       -> discard
//!   {"type":"done",        ...}                                       -> Done
//!   {"type":"error", "message":"...", ...}                            -> Protocol error
//! ```
//!
//! Cartesia's WS handshake doesn't accept custom auth headers from
//! browser clients, so the API key + version travel on the query
//! string. The proxy mirrors that convention.
//!
//! ## Audio format
//!
//! Cartesia's WS endpoint hard-rejects anything other than
//! `container: "raw"` (the proxy hit this on the first integration
//! smoke test). We therefore emit raw 16-bit signed-little-endian
//! mono PCM at 44.1 kHz, declared as
//! [`AudioFormat::Pcm_S16LE_44100_Mono`]. The silence splicer,
//! cache, replay endpoint, and browser playback are all
//! format-aware so the rest of the pipeline adapts.

use std::sync::Arc;

use async_stream::try_stream;
use async_trait::async_trait;
use base64::Engine;
use futures::{SinkExt, StreamExt};
use parley_core::chat::Cost;
use parley_core::tts::VoiceDescriptor;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use super::{
    AudioFormat, ProviderContinuationState, SynthesisContext, TtsChunk, TtsError, TtsProvider,
    TtsRequest, TtsStream,
};

/// Default WebSocket endpoint. Constructor takes an override so tests
/// can point at a local mock server.
pub const CARTESIA_TTS_WS_URL: &str = "wss://api.cartesia.ai/tts/websocket";

/// Default voices catalog endpoint (HTTP). 24-hour cache TTL matches
/// the contract used by xAI today.
pub const CARTESIA_TTS_VOICES_URL: &str = "https://api.cartesia.ai/voices";

/// API version pinned in the WS query string and the `/voices` HTTP
/// header. Bump explicitly when Cartesia ships a new dated version
/// rather than tracking "latest".
pub const CARTESIA_VERSION: &str = "2026-03-01";

/// Model id sent in the synthesis frame.
pub const CARTESIA_MODEL_ID: &str = "sonic-3";

/// Default voice. Katie — "stable, realistic" recommendation from
/// Cartesia's voice-agent guidance (spec §2).
pub const CARTESIA_DEFAULT_VOICE: &str = "f786b574-daa5-4673-aa0c-cbe3e8534c02";

/// Default BCP-47 language. English is all Parley speaks today.
pub const CARTESIA_DEFAULT_LANGUAGE: &str = "en";

/// PCM sample rate sent in the synthesis frame and declared via
/// [`AudioFormat::Pcm_S16LE_44100_Mono`].
pub const CARTESIA_SAMPLE_RATE: u32 = 44_100;

/// Per-character USD cost. Pinned to the Startup-tier effective rate
/// (~$30 / 1M chars). Spec §7.3 / §11 risk row "tier-dependent cost".
pub const CARTESIA_COST_PER_CHAR_USD: f64 = 30.0 / 1_000_000.0;

/// Curated voice entry. We surface a small, hand-picked set of
/// Cartesia voices in the picker rather than the full upstream
/// catalog; the catalog is large, mostly community-contributed, and
/// hard to compare quality on at a glance. Each entry pins the UUID
/// (used at synthesis time) and the human-readable label the picker
/// renders.
pub struct CuratedVoice {
    /// Cartesia voice id (UUID).
    pub id: &'static str,
    /// Display name shown in the dropdown.
    pub display_name: &'static str,
    /// BCP-47 language tag(s). Single English entry today; kept as a
    /// slice so future picks (e.g. multi-lingual voices) compose.
    pub language_tags: &'static [&'static str],
}

/// The curated short-list of Cartesia Sonic-3 voices Parley exposes
/// today. Source: Cartesia's "Choosing a Voice" guidance —
/// `docs.cartesia.ai/build-with-cartesia/tts-models/latest`. Mix of
/// stable agent-friendly voices (Katie, Kiefer) and emotive
/// expressive voices (Tessa, Kyle) so the user can A/B on a single
/// persona without scrolling a thousand-entry list.
///
/// Edit this list when a better-quality voice ships; the picker has
/// no opinions beyond what's listed here.
pub const CARTESIA_CURATED_VOICES: &[CuratedVoice] = &[
    CuratedVoice {
        id: "f786b574-daa5-4673-aa0c-cbe3e8534c02",
        display_name: "Katie (stable)",
        language_tags: &["en"],
    },
    CuratedVoice {
        id: "228fca29-3a0a-435c-8728-5cb483251068",
        display_name: "Kiefer (stable)",
        language_tags: &["en"],
    },
    CuratedVoice {
        id: "6ccbfb76-1fc6-48f7-b71d-91ac6298247b",
        display_name: "Tessa (emotive)",
        language_tags: &["en"],
    },
    CuratedVoice {
        id: "c961b81c-a935-4c17-bfb3-ba2239de8c2f",
        display_name: "Kyle (emotive)",
        language_tags: &["en"],
    },
];

/// Concrete Cartesia implementation of [`TtsProvider`]. Cheap to clone
/// — state is just an `Arc<str>` key, two endpoint strings, and a
/// shared voices cache.
#[derive(Clone)]
pub struct CartesiaTts {
    api_key: Arc<str>,
    ws_url: Arc<str>,
    voices_endpoint: Arc<str>,
    client: reqwest::Client,
}

impl CartesiaTts {
    /// Build a provider pointed at the production endpoints.
    pub fn new(api_key: impl Into<String>, client: reqwest::Client) -> Self {
        Self::with_endpoints(
            api_key,
            CARTESIA_TTS_WS_URL,
            CARTESIA_TTS_VOICES_URL,
            client,
        )
    }

    /// Build a provider with explicit endpoints. Used by tests that
    /// stand up a `tokio::net::TcpListener`-backed WS server and a
    /// `wiremock` HTTP server.
    pub fn with_endpoints(
        api_key: impl Into<String>,
        ws_url: impl Into<String>,
        voices_endpoint: impl Into<String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            api_key: api_key.into().into(),
            ws_url: ws_url.into().into(),
            voices_endpoint: voices_endpoint.into().into(),
            client,
        }
    }

    /// Build the WS handshake URL with auth + version on the query
    /// string. Pure string composition.
    fn handshake_url(&self) -> String {
        format!(
            "{}?api_key={}&cartesia_version={}",
            self.ws_url,
            urlencoding::encode(&self.api_key),
            CARTESIA_VERSION,
        )
    }
}

/// Pure parser for one Cartesia WS text frame. Returns:
/// - `Ok(ParsedFrame::Audio(bytes))` for `{"type":"chunk", ...}`
/// - `Ok(ParsedFrame::Done)`         for `{"type":"done", ...}`
/// - `Ok(ParsedFrame::Discard)`      for ingested-but-discarded
///                                   frames (timestamps, flush_done,
///                                   future control frames).
/// - `Err(TtsError::Protocol(..))`   for `{"type":"error", ...}` and
///                                   bad JSON / bad base64.
///
/// Factored out so the frame-handling table is testable without
/// standing up a real WebSocket server.
#[derive(Debug, PartialEq)]
enum ParsedFrame {
    Audio(Vec<u8>),
    Discard,
    Done,
}

fn parse_text_frame(text: &str) -> Result<ParsedFrame, TtsError> {
    let v: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| TtsError::Protocol(format!("ws text not json: {e}")))?;
    let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match kind {
        "chunk" => {
            let b64 = v
                .get("data")
                .and_then(|d| d.as_str())
                .ok_or_else(|| TtsError::Protocol("chunk frame missing data".into()))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| TtsError::Protocol(format!("base64 decode: {e}")))?;
            Ok(ParsedFrame::Audio(bytes))
        }
        "done" => Ok(ParsedFrame::Done),
        // Word-level timing isn't surfaced to clients today; flush
        // acknowledgments aren't user-visible. Drop and keep reading.
        "timestamps" | "flush_done" => Ok(ParsedFrame::Discard),
        "error" => {
            // Cartesia error frames carry both `title` and `message`
            // plus `error_code`. Surface message + code so the
            // operator can map back to the docs.
            let msg = v
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown");
            let code = v
                .get("error_code")
                .and_then(|c| c.as_str())
                .unwrap_or("unknown_error");
            Err(TtsError::Protocol(format!(
                "cartesia error [{code}]: {msg}"
            )))
        }
        // Forward-compat: future Cartesia control frames shouldn't
        // crash the reader. Discard and keep consuming.
        _ => Ok(ParsedFrame::Discard),
    }
}

/// Build the JSON synthesis frame sent on connection open. Pure
/// function so request-shape regressions are caught at unit-test
/// speed.
fn build_synthesis_frame(
    transcript: &str,
    voice_id: &str,
    context_id: &str,
    continue_: bool,
) -> serde_json::Value {
    json!({
        "model_id": CARTESIA_MODEL_ID,
        "transcript": transcript,
        "voice": { "mode": "id", "id": voice_id },
        "language": CARTESIA_DEFAULT_LANGUAGE,
        "context_id": context_id,
        "continue": continue_,
        "add_timestamps": false,
        "output_format": {
            "container": "raw",
            "encoding":  "pcm_s16le",
            "sample_rate": CARTESIA_SAMPLE_RATE,
        },
    })
}

/// Decide the `context_id` and `continue` flag for a synthesis call
/// based on the orchestrator's `SynthesisContext`. We thread a fresh
/// UUID at turn boundaries and reuse the prior chunk's id within a
/// turn.
fn resolve_context(ctx: &SynthesisContext) -> (String, bool) {
    match ctx.provider_state.as_ref() {
        Some(ProviderContinuationState::Cartesia(id))
            if ctx.previous_text.as_deref().is_some_and(|t| !t.is_empty()) =>
        {
            (id.clone(), true)
        }
        _ => (Uuid::new_v4().to_string(), false),
    }
}

#[async_trait]
impl TtsProvider for CartesiaTts {
    fn id(&self) -> &'static str {
        "cartesia"
    }

    async fn synthesize(
        &self,
        request: TtsRequest,
        ctx: SynthesisContext,
    ) -> Result<TtsStream, TtsError> {
        if request.text.is_empty() {
            return Err(TtsError::Other("empty text".into()));
        }
        let characters = request.text.chars().count() as u32;

        let voice = if request.voice_id.is_empty() {
            CARTESIA_DEFAULT_VOICE.to_string()
        } else {
            request.voice_id
        };

        let (context_id, continue_) = resolve_context(&ctx);
        let frame = build_synthesis_frame(&request.text, &voice, &context_id, continue_);

        let url = self.handshake_url();
        let (ws, _resp) = tokio_tungstenite::connect_async(&url).await.map_err(|e| {
            // tungstenite's error type lumps connect failures and
            // upgrade-rejected status codes together. We project bad
            // upgrade responses onto `TtsError::Http` (matching xAI
            // and ElevenLabs' HTTP behaviour) and everything else
            // onto `Transport`.
            if let tokio_tungstenite::tungstenite::Error::Http(resp) = &e {
                let status = resp.status().as_u16();
                let body = resp
                    .body()
                    .as_ref()
                    .map(|b| String::from_utf8_lossy(b).to_string())
                    .unwrap_or_default();
                TtsError::Http { status, body }
            } else {
                TtsError::Transport(e.to_string())
            }
        })?;

        let (mut sink, mut stream_rx) = ws.split();
        sink.send(Message::Text(frame.to_string()))
            .await
            .map_err(|e| TtsError::Transport(e.to_string()))?;

        let stream = try_stream! {
            while let Some(msg) = stream_rx.next().await {
                let msg = msg.map_err(|e| TtsError::Transport(e.to_string()))?;
                match msg {
                    Message::Text(text) => {
                        match parse_text_frame(&text)? {
                            ParsedFrame::Audio(bytes) => {
                                if !bytes.is_empty() {
                                    yield TtsChunk::Audio(bytes);
                                }
                            }
                            ParsedFrame::Done => {
                                yield TtsChunk::Done { characters };
                                break;
                            }
                            ParsedFrame::Discard => continue,
                        }
                    }
                    // Cartesia is documented to use only text frames
                    // for this endpoint, but we tolerate stray
                    // pings/pongs without erroring. Binary frames
                    // would be unexpected; drop them with no audio
                    // yield so a future protocol change doesn't
                    // crash the reader.
                    Message::Binary(_) | Message::Ping(_) | Message::Pong(_)
                    | Message::Frame(_) => continue,
                    Message::Close(_) => break,
                }
            }
        };
        Ok(Box::pin(stream))
    }

    fn cost(&self, characters: u32) -> Cost {
        Cost::from_usd(characters as f64 * CARTESIA_COST_PER_CHAR_USD)
    }

    fn output_format(&self) -> AudioFormat {
        AudioFormat::Pcm_S16LE_44100_Mono
    }

    fn supports_expressive_tags(&self) -> bool {
        // Sonic-3 honours `[laughter]` plus SSML volume/speed/emotion
        // tags. Spec §8.2.
        true
    }

    fn translate_expression_tags(&self, text: &str) -> String {
        translate_for_cartesia(text)
    }

    async fn voices(&self) -> Result<Vec<VoiceDescriptor>, TtsError> {
        // Cartesia's full catalog is large and quality-varied, so we
        // surface a curated short-list instead of the upstream
        // response. See [`CARTESIA_CURATED_VOICES`]. Synchronous +
        // allocation-only — no HTTP round-trip per picker render.
        Ok(CARTESIA_CURATED_VOICES
            .iter()
            .map(|v| VoiceDescriptor {
                id: v.id.to_string(),
                display_name: v.display_name.to_string(),
                language_tags: v.language_tags.iter().map(|s| s.to_string()).collect(),
            })
            .collect())
    }
}

/// Map Parley's neutral expression tags to Cartesia Sonic-3's native
/// markup. Cartesia accepts inline `[laughter]`, `[sigh]`, plus
/// SSML-style `<break time="…ms"/>` for explicit pauses (per
/// `docs/cartesia-sonic-3-integration-spec.md` §8.2). Wrapper-style
/// emotion tags (e.g. `<emotion value="excited">…</emotion>`) need a
/// scoped open/close pair we can't infer from a point-insertion neutral
/// tag, so emotion-flavoured cues fall back to plain stripping —
/// Sonic-3's voice still reads the text with natural inflection.
///
/// Pure / synchronous; called once per chunk on the dispatch hot path.
fn translate_for_cartesia(text: &str) -> String {
    use parley_core::expression::{Segment, split_into_segments};

    let mut out = String::with_capacity(text.len());
    for seg in split_into_segments(text) {
        match seg {
            Segment::Text(t) => out.push_str(t),
            Segment::Tag(id) => match id {
                "laugh" => out.push_str("[laughter]"),
                "sigh" => out.push_str("[sigh]"),
                "pause:short" => out.push_str(r#"<break time="250ms"/>"#),
                "pause:medium" => out.push_str(r#"<break time="700ms"/>"#),
                "pause:long" => out.push_str(r#"<break time="1500ms"/>"#),
                // Emotion-flavoured cues without an obvious scope —
                // strip and rely on the voice's natural prosody.
                _ => {}
            },
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    // ── pure helpers ───────────────────────────────────────────────

    #[test]
    fn id_returns_stable_string() {
        let p = CartesiaTts::new("k", reqwest::Client::new());
        assert_eq!(p.id(), "cartesia");
    }

    #[test]
    fn output_format_is_pcm_s16le_44100_mono() {
        let p = CartesiaTts::new("k", reqwest::Client::new());
        assert_eq!(p.output_format(), AudioFormat::Pcm_S16LE_44100_Mono);
    }

    #[test]
    fn supports_expressive_tags_is_true() {
        let p = CartesiaTts::new("k", reqwest::Client::new());
        assert!(p.supports_expressive_tags());
    }

    #[test]
    fn translate_laugh_to_native_inline_tag() {
        let p = CartesiaTts::new("k", reqwest::Client::new());
        assert_eq!(
            p.translate_expression_tags("That's funny {laugh} really."),
            "That's funny [laughter] really.",
        );
    }

    #[test]
    fn translate_pauses_to_ssml_breaks() {
        let p = CartesiaTts::new("k", reqwest::Client::new());
        assert_eq!(
            p.translate_expression_tags("hmm {pause:short} ok {pause:long} done"),
            r#"hmm <break time="250ms"/> ok <break time="1500ms"/> done"#,
        );
    }

    #[test]
    fn translate_strips_unsupported_emotion_tags() {
        let p = CartesiaTts::new("k", reqwest::Client::new());
        assert_eq!(
            p.translate_expression_tags("{warm}Hello there.{empathetic} I see."),
            "Hello there. I see.",
        );
    }

    #[test]
    fn cost_uses_pinned_per_char_rate() {
        let p = CartesiaTts::new("k", reqwest::Client::new());
        let c = p.cost(1000);
        assert!((c.usd - 0.030).abs() < 1e-9, "got {}", c.usd);
        let c = p.cost(1_000_000);
        assert!((c.usd - 30.0).abs() < 1e-9, "got {}", c.usd);
    }

    #[test]
    fn handshake_url_includes_api_key_and_version() {
        let p = CartesiaTts::with_endpoints(
            "secret-key",
            "wss://example.test/tts/websocket",
            "https://example.test/voices",
            reqwest::Client::new(),
        );
        let url = p.handshake_url();
        assert!(url.contains("api_key=secret-key"), "got {url}");
        assert!(
            url.contains(&format!("cartesia_version={CARTESIA_VERSION}")),
            "got {url}"
        );
    }

    #[test]
    fn handshake_url_url_encodes_api_key() {
        let p = CartesiaTts::with_endpoints(
            "key with spaces & symbols",
            "wss://example.test/tts/websocket",
            "https://example.test/voices",
            reqwest::Client::new(),
        );
        let url = p.handshake_url();
        assert!(url.contains("api_key=key%20with%20spaces%20%26%20symbols"));
    }

    #[tokio::test]
    async fn voices_returns_curated_short_list() {
        let p = CartesiaTts::new("k", reqwest::Client::new());
        let voices = p.voices().await.expect("curated voices");
        assert_eq!(
            voices.len(),
            CARTESIA_CURATED_VOICES.len(),
            "expected the curated list to be returned verbatim",
        );
        let names: Vec<&str> = voices.iter().map(|v| v.display_name.as_str()).collect();
        assert!(names.iter().any(|n| n.starts_with("Katie")));
        assert!(names.iter().any(|n| n.starts_with("Kiefer")));
        assert!(names.iter().any(|n| n.starts_with("Tessa")));
        assert!(names.iter().any(|n| n.starts_with("Kyle")));
        for v in &voices {
            assert!(!v.id.is_empty());
            assert_eq!(v.language_tags, vec!["en".to_string()]);
        }
    }

    #[test]
    fn synthesis_frame_uses_raw_pcm_s16le_output_format() {
        let frame = build_synthesis_frame("hello", "voice-uuid", "ctx-1", false);
        assert_eq!(frame["model_id"], "sonic-3");
        assert_eq!(frame["transcript"], "hello");
        assert_eq!(frame["voice"]["mode"], "id");
        assert_eq!(frame["voice"]["id"], "voice-uuid");
        assert_eq!(frame["language"], CARTESIA_DEFAULT_LANGUAGE);
        assert_eq!(frame["context_id"], "ctx-1");
        assert_eq!(frame["continue"], false);
        assert_eq!(frame["add_timestamps"], false);
        assert_eq!(frame["output_format"]["container"], "raw");
        assert_eq!(frame["output_format"]["encoding"], "pcm_s16le");
        assert_eq!(frame["output_format"]["sample_rate"], 44100);
        // No bit_rate field — that's an MP3-only knob.
        assert!(frame["output_format"].get("bit_rate").is_none());
    }

    #[test]
    fn synthesis_frame_emits_continue_true_for_continuation() {
        let frame = build_synthesis_frame("more", "v", "shared-ctx", true);
        assert_eq!(frame["continue"], true);
        assert_eq!(frame["context_id"], "shared-ctx");
    }

    // ── resolve_context ────────────────────────────────────────────

    #[test]
    fn resolve_context_fresh_when_no_prior_state() {
        let ctx = SynthesisContext::default();
        let (id, cont) = resolve_context(&ctx);
        assert!(!cont);
        assert!(!id.is_empty());
    }

    #[test]
    fn resolve_context_fresh_when_previous_text_empty() {
        let ctx = SynthesisContext {
            previous_text: Some(String::new()),
            provider_state: Some(ProviderContinuationState::Cartesia("old-ctx".into())),
            ..Default::default()
        };
        let (id, cont) = resolve_context(&ctx);
        assert!(!cont);
        assert_ne!(id, "old-ctx");
    }

    #[test]
    fn resolve_context_reuses_id_when_continuing() {
        let ctx = SynthesisContext {
            previous_text: Some("first chunk".into()),
            provider_state: Some(ProviderContinuationState::Cartesia("shared".into())),
            ..Default::default()
        };
        let (id, cont) = resolve_context(&ctx);
        assert!(cont);
        assert_eq!(id, "shared");
    }

    #[test]
    fn resolve_context_fresh_when_provider_state_is_other_variant() {
        let ctx = SynthesisContext {
            previous_text: Some("first".into()),
            provider_state: Some(ProviderContinuationState::ElevenLabsRequestId(
                "el-rid".into(),
            )),
            ..Default::default()
        };
        let (id, cont) = resolve_context(&ctx);
        assert!(!cont);
        assert_ne!(id, "el-rid");
    }

    // ── parse_text_frame ───────────────────────────────────────────

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn parse_text_frame_chunk_decodes_base64_audio() {
        let payload = b64(b"\x01\x02\x03\xff");
        let frame =
            format!(r#"{{"type":"chunk","data":"{payload}","done":false,"context_id":"x"}}"#);
        match parse_text_frame(&frame).unwrap() {
            ParsedFrame::Audio(bytes) => assert_eq!(bytes, b"\x01\x02\x03\xff"),
            other => panic!("expected Audio, got {other:?}"),
        }
    }

    #[test]
    fn parse_text_frame_chunk_with_empty_data_returns_empty_audio() {
        // Cartesia occasionally emits zero-length keepalive chunks
        // (status_code=206 with empty data). The reader should keep
        // going.
        let frame = r#"{"type":"chunk","data":"","done":false}"#;
        match parse_text_frame(frame).unwrap() {
            ParsedFrame::Audio(bytes) => assert!(bytes.is_empty()),
            other => panic!("expected Audio (empty), got {other:?}"),
        }
    }

    #[test]
    fn parse_text_frame_chunk_missing_data_is_protocol() {
        // Defense in depth — a malformed chunk frame with no `data`
        // field shouldn't silently emit zero audio.
        let frame = r#"{"type":"chunk","done":false}"#;
        match parse_text_frame(frame).unwrap_err() {
            TtsError::Protocol(_) => {}
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    #[test]
    fn parse_text_frame_chunk_bad_base64_is_protocol() {
        let frame = r#"{"type":"chunk","data":"!!!notbase64!!!","done":false}"#;
        match parse_text_frame(frame).unwrap_err() {
            TtsError::Protocol(msg) => assert!(msg.contains("base64"), "got {msg}"),
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    #[test]
    fn parse_text_frame_done() {
        let r = parse_text_frame(r#"{"type":"done","done":true}"#).unwrap();
        assert_eq!(r, ParsedFrame::Done);
    }

    #[test]
    fn parse_text_frame_timestamps_discarded() {
        let r = parse_text_frame(
            r#"{"type":"timestamps","done":false,"context_id":"x","word_timestamps":{"words":["hi"],"start":[0.0],"end":[0.2]}}"#,
        )
        .unwrap();
        assert_eq!(r, ParsedFrame::Discard);
    }

    #[test]
    fn parse_text_frame_flush_done_discarded() {
        let r = parse_text_frame(r#"{"type":"flush_done","flush_id":1}"#).unwrap();
        assert_eq!(r, ParsedFrame::Discard);
    }

    #[test]
    fn parse_text_frame_error_surfaces_message_and_code() {
        let frame = r#"{"type":"error","title":"Bad model","message":"sonic-3 not found","error_code":"model_not_found","status_code":400,"context_id":"x"}"#;
        match parse_text_frame(frame).unwrap_err() {
            TtsError::Protocol(m) => {
                assert!(m.contains("sonic-3 not found"), "got {m}");
                assert!(m.contains("model_not_found"), "got {m}");
            }
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    #[test]
    fn parse_text_frame_unknown_type_is_discarded() {
        let r = parse_text_frame(r#"{"type":"future_thing","ack":true}"#).unwrap();
        assert_eq!(r, ParsedFrame::Discard);
    }

    #[test]
    fn parse_text_frame_bad_json_is_protocol() {
        match parse_text_frame("not json").unwrap_err() {
            TtsError::Protocol(_) => {}
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    // ── synthesize() ───────────────────────────────────────────────

    #[tokio::test]
    async fn empty_text_is_rejected_locally_without_ws() {
        let p = CartesiaTts::with_endpoints(
            "k",
            "ws://127.0.0.1:1/will-never-connect",
            "http://example.test/voices",
            reqwest::Client::new(),
        );
        let r = p
            .synthesize(
                TtsRequest {
                    voice_id: "v".into(),
                    text: String::new(),
                },
                SynthesisContext::default(),
            )
            .await;
        match r {
            Err(TtsError::Other(_)) => {}
            Err(e) => panic!("expected Other, got {e:?}"),
            Ok(_) => panic!("expected Other, got Ok stream"),
        }
    }

    /// Run a tiny WS server that:
    /// 1. Reads exactly one text frame (the synthesis request).
    /// 2. Sends each scripted server message in order.
    /// 3. Closes the connection.
    async fn spawn_mock_ws(
        scripted: Vec<Message>,
    ) -> (String, tokio::task::JoinHandle<Option<String>>) {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        // Trailing `/` matters: a bare `ws://host:port` URL gets
        // rejected by tungstenite's URL validation in some versions.
        let url = format!("ws://{addr}/");

        let handle = tokio::spawn(async move {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[mock-ws] accept failed: {e}");
                    return None;
                }
            };
            let mut ws = match tokio_tungstenite::accept_async(stream).await {
                Ok(ws) => ws,
                Err(e) => {
                    eprintln!("[mock-ws] handshake failed: {e}");
                    return None;
                }
            };
            let captured = match ws.next().await {
                Some(Ok(Message::Text(t))) => Some(t),
                Some(Ok(other)) => {
                    eprintln!("[mock-ws] unexpected first frame: {other:?}");
                    None
                }
                Some(Err(e)) => {
                    eprintln!("[mock-ws] read error: {e}");
                    None
                }
                None => {
                    eprintln!("[mock-ws] stream ended before first frame");
                    None
                }
            };
            for msg in scripted {
                if let Err(e) = ws.send(msg).await {
                    eprintln!("[mock-ws] send error: {e}");
                    return captured;
                }
            }
            let _ = ws.send(Message::Close(None)).await;
            captured
        });

        (url, handle)
    }

    /// Build a Cartesia `chunk` text frame that the real server
    /// would emit.
    fn server_chunk(audio: &[u8]) -> Message {
        Message::Text(
            json!({
                "type": "chunk",
                "data": b64(audio),
                "done": false,
                "status_code": 206,
                "context_id": "ctx",
            })
            .to_string(),
        )
    }

    fn server_done() -> Message {
        Message::Text(
            json!({
                "type": "done",
                "done": true,
                "status_code": 206,
                "context_id": "ctx",
            })
            .to_string(),
        )
    }

    fn server_error(code: &str, message: &str) -> Message {
        Message::Text(
            json!({
                "type": "error",
                "title": "Mock error",
                "message": message,
                "error_code": code,
                "status_code": 400,
                "context_id": "ctx",
            })
            .to_string(),
        )
    }

    /// Drain a `TtsStream` to (audio_bytes, saw_done, last_err).
    async fn drain(mut s: TtsStream) -> (Vec<u8>, bool, Option<TtsError>) {
        let mut audio = Vec::new();
        let mut saw_done = false;
        let mut last_err = None;
        while let Some(item) = s.next().await {
            match item {
                Ok(TtsChunk::Audio(b)) => audio.extend(b),
                Ok(TtsChunk::Done { .. }) => saw_done = true,
                Err(e) => {
                    last_err = Some(e);
                    break;
                }
            }
        }
        (audio, saw_done, last_err)
    }

    #[tokio::test]
    async fn synthesize_emits_audio_then_done() {
        // Two PCM "samples" worth of bytes — content is opaque to
        // the splicer, just bytes-through.
        let payload = b"\x00\x01\x02\x03\x10\x20";
        let (url, server_handle) = spawn_mock_ws(vec![server_chunk(payload), server_done()]).await;

        let p = CartesiaTts::with_endpoints(
            "test-key",
            url,
            "http://example.test/voices",
            reqwest::Client::new(),
        );
        let stream = p
            .synthesize(
                TtsRequest {
                    voice_id: "voice-1".into(),
                    text: "hello world".into(),
                },
                SynthesisContext::default(),
            )
            .await
            .unwrap();
        let (audio, saw_done, err) = drain(stream).await;
        assert!(saw_done, "expected Done frame");
        assert!(err.is_none(), "got {err:?}");
        assert_eq!(audio, payload);

        let captured = server_handle.await.unwrap().expect("captured frame");
        let parsed: serde_json::Value = serde_json::from_str(&captured).unwrap();
        assert_eq!(parsed["transcript"], "hello world");
        assert_eq!(parsed["voice"]["mode"], "id");
        assert_eq!(parsed["voice"]["id"], "voice-1");
        assert_eq!(parsed["output_format"]["container"], "raw");
        assert_eq!(parsed["output_format"]["encoding"], "pcm_s16le");
        assert_eq!(parsed["output_format"]["sample_rate"], 44100);
    }

    #[tokio::test]
    async fn synthesize_concatenates_multiple_chunks() {
        // Realistic shape: many chunk frames before done.
        let (url, _) = spawn_mock_ws(vec![
            server_chunk(b"AAAA"),
            server_chunk(b"BBBB"),
            server_chunk(b"CCCC"),
            server_done(),
        ])
        .await;
        let p = CartesiaTts::with_endpoints(
            "k",
            url,
            "http://example.test/voices",
            reqwest::Client::new(),
        );
        let stream = p
            .synthesize(
                TtsRequest {
                    voice_id: "v".into(),
                    text: "x".into(),
                },
                SynthesisContext::default(),
            )
            .await
            .unwrap();
        let (audio, saw_done, err) = drain(stream).await;
        assert!(saw_done);
        assert!(err.is_none(), "got {err:?}");
        assert_eq!(audio, b"AAAABBBBCCCC");
    }

    #[tokio::test]
    async fn synthesize_uses_default_voice_when_id_empty() {
        let (url, server_handle) = spawn_mock_ws(vec![server_done()]).await;
        let p = CartesiaTts::with_endpoints(
            "k",
            url,
            "http://example.test/voices",
            reqwest::Client::new(),
        );
        let stream = p
            .synthesize(
                TtsRequest {
                    voice_id: String::new(),
                    text: "x".into(),
                },
                SynthesisContext::default(),
            )
            .await
            .unwrap();
        let _ = drain(stream).await;
        let captured = server_handle.await.unwrap().expect("captured frame");
        let parsed: serde_json::Value = serde_json::from_str(&captured).unwrap();
        assert_eq!(parsed["voice"]["id"], CARTESIA_DEFAULT_VOICE);
    }

    #[tokio::test]
    async fn synthesize_fresh_context_when_no_previous_text() {
        let (url, server_handle) = spawn_mock_ws(vec![server_done()]).await;
        let p = CartesiaTts::with_endpoints(
            "k",
            url,
            "http://example.test/voices",
            reqwest::Client::new(),
        );
        let stream = p
            .synthesize(
                TtsRequest {
                    voice_id: "v".into(),
                    text: "first".into(),
                },
                SynthesisContext::default(),
            )
            .await
            .unwrap();
        let _ = drain(stream).await;
        let captured = server_handle.await.unwrap().expect("captured frame");
        let parsed: serde_json::Value = serde_json::from_str(&captured).unwrap();
        assert_eq!(parsed["continue"], false);
    }

    #[tokio::test]
    async fn synthesize_continuation_sets_continue_true_and_reuses_context_id() {
        let (url, server_handle) = spawn_mock_ws(vec![server_done()]).await;
        let p = CartesiaTts::with_endpoints(
            "k",
            url,
            "http://example.test/voices",
            reqwest::Client::new(),
        );
        let stream = p
            .synthesize(
                TtsRequest {
                    voice_id: "v".into(),
                    text: "second".into(),
                },
                SynthesisContext {
                    previous_text: Some("first".into()),
                    provider_state: Some(ProviderContinuationState::Cartesia("shared-ctx".into())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let _ = drain(stream).await;
        let captured = server_handle.await.unwrap().expect("captured frame");
        let parsed: serde_json::Value = serde_json::from_str(&captured).unwrap();
        assert_eq!(parsed["context_id"], "shared-ctx");
        assert_eq!(parsed["continue"], true);
    }

    #[tokio::test]
    async fn synthesize_error_frame_surfaces_protocol_error() {
        let (url, _server) =
            spawn_mock_ws(vec![server_error("bad_voice", "voice not found")]).await;
        let p = CartesiaTts::with_endpoints(
            "k",
            url,
            "http://example.test/voices",
            reqwest::Client::new(),
        );
        let stream = p
            .synthesize(
                TtsRequest {
                    voice_id: "v".into(),
                    text: "x".into(),
                },
                SynthesisContext::default(),
            )
            .await
            .unwrap();
        let (_audio, _saw_done, err) = drain(stream).await;
        match err {
            Some(TtsError::Protocol(m)) => {
                assert!(m.contains("voice not found"), "got {m}");
                assert!(m.contains("bad_voice"), "got {m}");
            }
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn synthesize_timestamps_and_flush_done_frames_are_discarded() {
        // Cartesia interleaves audio chunks with optional metadata
        // frames. The reader should pass audio through and drop the
        // metadata frames silently.
        let (url, _server) = spawn_mock_ws(vec![
            server_chunk(b"audio-1"),
            Message::Text(
                json!({
                    "type": "timestamps",
                    "done": false,
                    "context_id": "ctx",
                    "word_timestamps": {
                        "words": ["hi"],
                        "start": [0.0],
                        "end": [0.2],
                    }
                })
                .to_string(),
            ),
            Message::Text(
                json!({
                    "type": "flush_done",
                    "flush_id": 1,
                    "flush_done": true,
                    "context_id": "ctx",
                })
                .to_string(),
            ),
            server_chunk(b"audio-2"),
            server_done(),
        ])
        .await;
        let p = CartesiaTts::with_endpoints(
            "k",
            url,
            "http://example.test/voices",
            reqwest::Client::new(),
        );
        let stream = p
            .synthesize(
                TtsRequest {
                    voice_id: "v".into(),
                    text: "x".into(),
                },
                SynthesisContext::default(),
            )
            .await
            .unwrap();
        let (audio, saw_done, err) = drain(stream).await;
        assert!(err.is_none(), "got {err:?}");
        assert!(saw_done);
        assert_eq!(audio, b"audio-1audio-2");
    }

    #[tokio::test]
    async fn synthesize_ws_handshake_failure_is_transport_error() {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}/");
        tokio::spawn(async move {
            let _ = listener.accept().await; // accept and drop without handshake
        });

        let p = CartesiaTts::with_endpoints(
            "k",
            url,
            "http://example.test/voices",
            reqwest::Client::new(),
        );
        let r = p
            .synthesize(
                TtsRequest {
                    voice_id: "v".into(),
                    text: "x".into(),
                },
                SynthesisContext::default(),
            )
            .await;
        match r {
            Err(TtsError::Transport(_)) => {}
            Err(e) => panic!("expected Transport, got {e:?}"),
            Ok(_) => panic!("expected Transport, got Ok stream"),
        }
    }

    // The previous mock-server tests covering the upstream `/voices`
    // HTTP envelope parser are gone — `voices()` now returns the
    // curated short-list (`CARTESIA_CURATED_VOICES`) directly. The
    // curated-list contract is exercised by
    // `voices_returns_curated_short_list` near the top of this mod.
}
