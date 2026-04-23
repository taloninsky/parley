//! Streaming text-to-speech providers.
//!
//! Mirrors the `llm` module: a thin trait + per-provider impl, with
//! the orchestrator owning the dispatch loop. The chunker that
//! decides what text to send lives in `parley-core::tts` so it can
//! be unit-tested across the workspace.
//!
//! Spec: `docs/conversation-voice-slice-spec.md` §4.2 and §6.

pub mod cache;
pub mod elevenlabs;
pub mod hub;
pub mod silence;

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use parley_core::chat::Cost;
use thiserror::Error;

// Re-exports kept here so the orchestrator and HTTP layer can
// reference `crate::tts::Foo` instead of going through the
// submodule path. `TtsCacheReader` and `ElevenLabsTts` aren't yet
// referenced through the re-export (only by absolute path / from
// startup wiring landing in Task 8); allow the dead-code lint until
// then.
#[allow(unused_imports)]
pub use cache::{FsTtsCache, TtsCacheReader, TtsCacheWriter};
#[allow(unused_imports)]
pub use elevenlabs::ElevenLabsTts;
pub use hub::{TtsBroadcastFrame, TtsBroadcaster, TtsHub};

/// One synthesis request handed to a [`TtsProvider`]. The
/// orchestrator builds these from [`parley_core::tts::SentenceChunk`]
/// values; we do not pass the chunk type through the trait to keep
/// the provider crate-agnostic about chunking policy.
#[derive(Debug, Clone)]
pub struct TtsRequest {
    /// Voice identifier as understood by the provider. For ElevenLabs
    /// this is the voice id (e.g. `"c6SfcYrb2t09NHXiT80T"` for
    /// "Jarnathan").
    pub voice_id: String,
    /// Text to synthesize. Must be non-empty; the provider may reject
    /// or truncate empty strings.
    pub text: String,
}

/// One frame of a streaming TTS response. Audio frames carry raw
/// container bytes (MP3 for ElevenLabs); the terminal `Done` frame
/// reports billing-grade character counts so the orchestrator can
/// finalize cost.
#[derive(Debug, Clone)]
pub enum TtsChunk {
    /// Raw audio bytes ready for the cache file and the live SSE
    /// fan-out. Container framing (MP3) is preserved verbatim.
    Audio(Vec<u8>),
    /// End-of-stream marker. `characters` is the *billable* count
    /// (typically the input text length); the orchestrator uses it
    /// to compute cost via [`TtsProvider::cost`].
    Done {
        /// Billable character count for this request.
        characters: u32,
    },
}

/// Errors a [`TtsProvider`] can surface. Distinct from `LlmError`
/// because TTS failures should not roll back the LLM turn — the
/// orchestrator will catch these and emit a non-fatal `Failed` event
/// while still appending the AI turn to the session.
#[derive(Debug, Error)]
pub enum TtsError {
    /// Network or transport failure talking to the provider.
    #[error("tts transport error: {0}")]
    Transport(String),
    /// Provider returned a non-success HTTP status. `status` is the
    /// HTTP code; `body` is the response body (truncated by the
    /// caller if large).
    #[error("tts http error: status={status}, body={body}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Response body as a string (best-effort decode).
        body: String,
    },
    /// Provider response could not be parsed (e.g. invalid event
    /// frame, unexpected EOF mid-chunk).
    #[error("tts protocol error: {0}")]
    Protocol(String),
    /// Catch-all for provider-specific failures that don't fit the
    /// above buckets.
    #[error("tts error: {0}")]
    Other(String),
}

/// Stream of audio chunks plus a terminal `Done`. Modeled exactly
/// like `LlmProvider::stream_chat` so the orchestrator's loop shape
/// is symmetric.
pub type TtsStream = Pin<Box<dyn Stream<Item = Result<TtsChunk, TtsError>> + Send>>;

/// Audio container/codec shape a [`TtsProvider`] returns. Today only
/// 128 kbps stereo MP3 at 44.1 kHz is implemented (ElevenLabs); the
/// enum exists so the `SilenceSplicer` can pick a matching silence
/// frame and so future providers can declare a different format
/// without leaking through the orchestrator.
///
/// Spec: `docs/paragraph-tts-chunking-spec.md` §3.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    /// 44.1 kHz, 128 kbps, stereo MP3 (CBR). ElevenLabs default.
    Mp3_44100_128,
}

/// Provider-specific opaque continuation handle carried inside a
/// [`SynthesisContext`]. Each provider that wants cross-chunk
/// continuity defines its own variant.
#[derive(Debug, Clone)]
pub enum ProviderContinuationState {
    /// ElevenLabs HTTP `request-id` header from the prior chunk's
    /// response. Used to populate `previous_request_ids` on the next
    /// request when the v2 stitching model is in play. The default
    /// adapter (v3) ignores this.
    ElevenLabsRequestId(String),
}

/// Cross-chunk continuity hints passed into [`TtsProvider::synthesize`].
/// All fields are advisory — providers that don't support a given
/// hint simply ignore it.
///
/// Spec: `docs/paragraph-tts-chunking-spec.md` §3.2.
#[derive(Debug, Clone, Default)]
pub struct SynthesisContext {
    /// Text of the immediately prior chunk in the same turn, if any.
    /// May be used as a `previous_text` hint for prosody.
    pub previous_text: Option<String>,
    /// Hint at the next chunk's text when the orchestrator already
    /// has it buffered (rare in pure streaming).
    pub next_text_hint: Option<String>,
    /// Zero-based index of this chunk within the turn.
    pub chunk_index: u32,
    /// `true` when this is the last chunk for the turn.
    pub final_for_turn: bool,
    /// Provider-specific opaque continuation state from the prior
    /// chunk's response.
    pub provider_state: Option<ProviderContinuationState>,
}

/// Streaming TTS provider. Implementations are constructed once at
/// proxy startup with their API key already resolved.
#[async_trait]
pub trait TtsProvider: Send + Sync {
    /// Stable provider id (e.g. `"elevenlabs"`). Used for logging
    /// and registry lookups; not the same shape as `LlmProvider::id`
    /// but plays the same role.
    fn id(&self) -> &'static str;

    /// Synthesize `request.text` in the requested voice. Returns a
    /// stream of audio chunks terminated by a single `Done`.
    ///
    /// `ctx` carries cross-chunk continuity hints. Providers that
    /// don't use a given hint must ignore it without error.
    async fn synthesize(
        &self,
        request: TtsRequest,
        ctx: SynthesisContext,
    ) -> Result<TtsStream, TtsError>;

    /// Audio format produced by [`Self::synthesize`]. Used by the
    /// `SilenceSplicer` to pick a matching silence frame.
    fn output_format(&self) -> AudioFormat;

    /// Whether this provider understands ElevenLabs-style expressive
    /// annotation tags (e.g. `[whisper]`, `[laugh]`). The annotator
    /// pass uses this to decide whether to inject tags into prompts.
    fn supports_expressive_tags(&self) -> bool;

    /// Compute the USD cost for `characters` synthesized in this
    /// provider's pricing tier. Pure function — no I/O.
    fn cost(&self, characters: u32) -> Cost;
}
