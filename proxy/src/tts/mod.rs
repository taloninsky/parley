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
    async fn synthesize(&self, request: TtsRequest) -> Result<TtsStream, TtsError>;

    /// Compute the USD cost for `characters` synthesized in this
    /// provider's pricing tier. Pure function — no I/O.
    fn cost(&self, characters: u32) -> Cost;
}
