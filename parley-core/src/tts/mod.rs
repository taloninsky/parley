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

pub mod chunking;
pub mod sentence;

pub use chunking::{ChunkPlanner, ChunkPolicy, ReleasedChunk};
pub use sentence::{SentenceChunk, SentenceChunker};

use serde::{Deserialize, Serialize};

/// One voice option a [`TtsProvider`] exposes via `voices()`. Used by
/// the frontend voice picker ([§9 voice_picker.rs]) and persisted in
/// the user profile as `tts.voice_id`.
///
/// `id` is the provider-native voice identifier (e.g. xAI's `"eve"`,
/// ElevenLabs' 20-char voice id).
///
/// Spec: `docs/xai-speech-integration-spec.md` §6.3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceDescriptor {
    /// Provider-native voice identifier sent back to the provider on
    /// synthesis.
    pub id: String,
    /// Human-friendly name shown in the UI (e.g. `"Eve"`, `"Jarnathan"`).
    pub display_name: String,
    /// BCP-47 language tags this voice is tuned for (e.g. `["en-US"]`).
    /// Empty when the provider doesn't advertise language affinity.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub language_tags: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_descriptor_round_trips() {
        let v = VoiceDescriptor {
            id: "eve".into(),
            display_name: "Eve".into(),
            language_tags: vec!["en-US".into()],
        };
        let json = serde_json::to_string(&v).unwrap();
        let parsed: VoiceDescriptor = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, v);
    }

    #[test]
    fn voice_descriptor_omits_empty_language_tags() {
        let v = VoiceDescriptor {
            id: "eve".into(),
            display_name: "Eve".into(),
            language_tags: vec![],
        };
        let json = serde_json::to_string(&v).unwrap();
        assert!(!json.contains("language_tags"));
    }
}
