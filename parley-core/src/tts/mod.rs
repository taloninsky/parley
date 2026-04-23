//! TTS-adjacent shared types and pure logic.
//!
//! Lives in `parley-core` (not the proxy) because the
//! [`SentenceChunker`] is the contract between the LLM token stream
//! and the TTS dispatcher, and we want it covered by unit tests on
//! any platform — including the WASM frontend if it ever needs to
//! preview chunking.
//!
//! The actual `TtsProvider` trait + HTTP plumbing live in the proxy
//! (same boundary as `LlmProvider`).
//!
//! Spec reference: `docs/conversation-voice-slice-spec.md` §4.1.

pub mod sentence;

pub use sentence::{SentenceChunk, SentenceChunker};
