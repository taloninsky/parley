//! LLM provider abstraction.
//!
//! The [`LlmProvider`] trait is the minimum surface every chat-LLM
//! implementation must expose so the conversation orchestrator can
//! drive a turn without provider-specific knowledge. It lives in the
//! proxy (not in `parley-core`) because:
//!
//! - The WASM frontend never calls a provider directly — it talks to
//!   the proxy over HTTP.
//! - Provider plumbing pulls in `async-trait`, `futures::Stream`, and
//!   HTTP machinery that has no business in the WASM bundle.
//! - Promoting a type from `proxy` into `parley-core` later is a
//!   mechanical move; demoting is much harder.
//!
//! Spec reference: `docs/conversation-mode-spec.md` §12.

// Foundational scaffolding for the conversation orchestrator. The
// trait + Anthropic impl have full unit coverage but no production
// callsite yet — the orchestrator slice is the next piece. Allow
// dead-code module-wide so we don't have to sprinkle attributes on
// every item only to remove them when the consumer lands.
#![allow(dead_code)]

use async_trait::async_trait;
use futures::stream::BoxStream;
use parley_core::chat::{ChatMessage, ChatToken, Cost, TokenUsage};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod anthropic;
pub mod sse;

/// Universal options for a single chat exchange. Provider-specific
/// knobs (Anthropic's `extended_thinking`, OpenAI's reasoning effort,
/// etc.) ride in `provider_extensions` so the trait stays uniform.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatOptions {
    /// Sampling temperature. `None` means "use provider default."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Hard cap on output tokens. `None` means "use provider default."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Stop sequences. Empty means "none."
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    /// Opaque provider-specific options. Forwarded verbatim by the
    /// implementation. Lets us expose Anthropic's `thinking` block,
    /// OpenAI's `reasoning_effort`, etc., without polluting the shared
    /// trait.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub provider_extensions: serde_json::Value,
}

/// All failure modes a provider call can produce. The orchestrator's
/// failure-handling path (spec §10.1) inspects the variant to decide
/// whether retry is sensible.
#[derive(Debug, Error)]
pub enum LlmError {
    /// Network or transport failure (DNS, TLS, connection reset).
    /// Generally retryable.
    #[error("transport error: {0}")]
    Transport(String),
    /// HTTP-level failure with a status code and body. Inspect status
    /// for retryability — 4xx is usually a misconfiguration, 5xx is
    /// usually transient.
    #[error("HTTP {status}: {body}")]
    Http {
        /// HTTP status code from the provider.
        status: u16,
        /// Response body (truncated at the call site if needed).
        body: String,
    },
    /// Provider returned a payload we couldn't parse.
    #[error("malformed response: {0}")]
    BadResponse(String),
    /// Auth failure — invalid or missing API key. Not retryable.
    #[error("authentication failed: {0}")]
    Auth(String),
    /// Catch-all for provider-specific errors that don't fit elsewhere.
    #[error("provider error: {0}")]
    Other(String),
}

/// Result alias for provider calls.
pub type LlmResult<T> = Result<T, LlmError>;

/// The minimum surface every LLM provider must implement. The
/// orchestrator interacts only through this trait when routing a turn;
/// per-persona configuration code (which already knows the concrete
/// provider) can use provider-specific extension traits for opt-in
/// affordances.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Stable identifier for this provider instance, e.g.
    /// `"anthropic:claude-haiku-4-5-20251001"`. Used for logging and
    /// for cost-table lookups.
    fn id(&self) -> &str;

    /// The model's context window in tokens. Drives compaction
    /// thresholds (spec §9.2).
    fn context_window(&self) -> u32;

    /// Approximate token count for the given text. Implementations
    /// should use the provider's native tokenizer when available;
    /// otherwise a word-count fallback is acceptable per spec §9.2.
    fn count_tokens(&self, text: &str) -> u64;

    /// USD cost of a completed exchange given final token usage.
    /// Computed from the configured per-million rates.
    fn cost(&self, usage: TokenUsage) -> Cost;

    /// Non-streaming chat completion. Returns the full response text
    /// plus token accounting. Suitable for short tool-style calls or
    /// for callers that don't care about first-token latency.
    async fn complete(
        &self,
        messages: &[ChatMessage],
        opts: &ChatOptions,
    ) -> LlmResult<ChatCompletion>;

    /// Streaming chat completion. Yields a sequence of [`ChatToken`]s
    /// as the provider streams its response, ending with exactly one
    /// `ChatToken::Done` carrying final accounting (when the provider
    /// reports it). Implementations must guarantee that the stream
    /// terminates — either with `Done` or with an error item — so the
    /// orchestrator's state machine can advance.
    async fn stream_chat(
        &self,
        messages: &[ChatMessage],
        opts: &ChatOptions,
    ) -> LlmResult<BoxStream<'static, LlmResult<ChatToken>>>;
}

/// Result of a non-streaming [`LlmProvider::complete`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatCompletion {
    /// Full response text emitted by the model.
    pub text: String,
    /// Final token accounting. The orchestrator multiplies this by the
    /// model's rates to compute USD cost (see [`LlmProvider::cost`]).
    pub usage: TokenUsage,
}
