//! Streaming / batch speech-to-text providers.
//!
//! Mirrors the shape of the [`crate::llm`] and [`crate::tts`] modules:
//! a thin trait + per-provider impl, with the orchestrator owning
//! dispatch. Canonical wire types (`SttRequest`, `Transcript`,
//! `TranscriptEvent`, `SttStreamConfig`) live in
//! [`parley_core::stt`] so they can travel between the WASM frontend
//! and the proxy without a format translation.
//!
//! Spec: `docs/xai-speech-integration-spec.md` §6.2, §6.6.

// Provider impls and trait exist but only become live when the
// orchestrator router lands in Step 6. Silence the dead-code lint
// module-wide until then.
#![allow(dead_code)]

pub mod xai;

use async_trait::async_trait;
use futures::stream::BoxStream;
use parley_core::chat::Cost;
use parley_core::stt::{SttRequest, SttStreamConfig, Transcript, TranscriptEvent};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::providers::ProviderId;

pub use xai::XaiStt;

/// All failure modes an STT provider call can surface. Distinct from
/// `LlmError` because an STT failure should not roll back a turn — the
/// orchestrator catches and reports these as non-fatal where possible.
#[derive(Debug, Error)]
pub enum SttError {
    /// Network or transport failure (DNS, TLS, connection reset).
    /// Generally retryable.
    #[error("transport error: {0}")]
    Transport(String),
    /// HTTP-level failure with status and body.
    #[error("HTTP {status}: {body}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Response body (truncated at the call site if large).
        body: String,
    },
    /// Provider returned a payload we couldn't parse.
    #[error("malformed response: {0}")]
    BadResponse(String),
    /// Auth failure — invalid or missing API key.
    #[error("authentication failed: {0}")]
    Auth(String),
    /// Streaming protocol violation (unexpected frame, missing
    /// terminator, etc.).
    #[error("protocol error: {0}")]
    Protocol(String),
    /// Feature not supported by this provider (e.g. requesting
    /// streaming from a provider that only does batch).
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// Catch-all for provider-specific failures that don't fit above.
    #[error("provider error: {0}")]
    Other(String),
}

/// Result alias for STT provider calls.
pub type SttResult<T> = Result<T, SttError>;

/// Handle returned from [`SttProvider::stream`]. The caller pushes raw
/// audio frames into `audio_tx` and reads transcript events from
/// `events`. Dropping `audio_tx` (or closing it explicitly) signals
/// end-of-input; the provider then drains any pending events and emits
/// a terminal [`TranscriptEvent::Done`] before closing `events`.
pub struct SttStreamHandle {
    /// Sink for raw audio frames (container shape is set by the
    /// [`SttStreamConfig::format`] the handle was opened with).
    pub audio_tx: mpsc::Sender<Vec<u8>>,
    /// Stream of transcript events terminating with one
    /// [`TranscriptEvent::Done`].
    pub events: BoxStream<'static, SttResult<TranscriptEvent>>,
}

/// The minimum surface every STT provider must implement.
///
/// `transcribe` is the file/batch path; `stream` is the realtime WS
/// path. Providers that don't do one or the other should return
/// [`SttError::Unsupported`].
#[async_trait]
pub trait SttProvider: Send + Sync {
    /// Stable provider id (matches the registry).
    fn id(&self) -> ProviderId;

    /// File / batch transcription. Input audio is consumed whole;
    /// output is a final [`Transcript`].
    async fn transcribe(&self, request: SttRequest) -> SttResult<Transcript>;

    /// Open a streaming session. Returns an [`SttStreamHandle`] pair:
    /// audio sink + event source. See [`SttStreamHandle`] for the
    /// lifecycle contract.
    async fn stream(&self, config: SttStreamConfig) -> SttResult<SttStreamHandle>;

    /// USD cost for `seconds` of input audio at this provider's
    /// current rates. `streaming=true` selects the streaming tier;
    /// `streaming=false` selects batch. Returns the shared
    /// [`parley_core::chat::Cost`] struct used by LLM and TTS cost so
    /// `TurnProvenance` carries all three uniformly.
    fn cost(&self, seconds: f64, streaming: bool) -> Cost;
}
