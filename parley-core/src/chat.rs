//! Chat exchange data types shared between the WASM frontend and the
//! native proxy. These are *data shapes*, not capabilities — they carry
//! no behavior beyond serialization. The actual `LlmProvider` trait
//! that consumes them lives in `parley-proxy`, because:
//!
//! 1. The WASM frontend never calls a provider directly; it talks to
//!    the proxy over HTTP.
//! 2. Provider plumbing pulls in `async-trait`, `futures`, and HTTP
//!    machinery that has no business in the WASM bundle.
//!
//! Spec references: `docs/conversation-mode-spec.md` §11 (cost),
//! §12.2 (shared trait surface).

use serde::{Deserialize, Serialize};

/// Conversational role for a single message in a chat exchange. Mirrors
/// the OpenAI/Anthropic shared vocabulary so messages round-trip
/// through any provider's API without remapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatRole {
    /// The system prompt — the persona instructions, expression-tag
    /// guidance, etc.
    System,
    /// A human turn (or the speaker-labeled aggregate of human turns).
    User,
    /// A prior AI response in the conversation history.
    Assistant,
}

/// One message in a chat exchange. Persistable: the conversation
/// history slice handed to the LLM is `&[ChatMessage]`, and the same
/// shape eventually gets archived in the session file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Who is speaking in this message.
    pub role: ChatRole,
    /// The message body. Pre-rendered text; expression annotations and
    /// speaker labels are already inlined by the orchestrator before
    /// the message reaches the provider.
    pub content: String,
}

impl ChatMessage {
    /// Convenience constructor for a system-prompt message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::System,
            content: content.into(),
        }
    }

    /// Convenience constructor for a user-turn message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
        }
    }

    /// Convenience constructor for a prior assistant turn.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Assistant,
            content: content.into(),
        }
    }
}

/// Token accounting for a completed exchange. Both fields are reported
/// by the provider; the orchestrator multiplies them by the model's
/// rates to compute cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Tokens consumed from the prompt (system + history + user turn).
    pub input: u64,
    /// Tokens emitted by the model.
    pub output: u64,
}

impl TokenUsage {
    /// Sum of input and output. Useful for context-window accounting.
    pub fn total(&self) -> u64 {
        self.input + self.output
    }
}

/// USD cost for a single exchange. Carried as a struct (rather than a
/// bare `f64`) so the call site can't accidentally add cost values
/// across currencies in some hypothetical future.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    /// USD amount, full precision.
    pub usd: f64,
}

impl Cost {
    /// Construct from a USD amount.
    pub fn from_usd(usd: f64) -> Self {
        Self { usd }
    }
}

impl std::ops::Add for Cost {
    type Output = Cost;
    fn add(self, rhs: Cost) -> Cost {
        Cost {
            usd: self.usd + rhs.usd,
        }
    }
}

impl std::ops::AddAssign for Cost {
    fn add_assign(&mut self, rhs: Cost) {
        self.usd += rhs.usd;
    }
}

/// One incremental delta from a streaming chat exchange. The proxy's
/// `LlmProvider::stream_chat` yields a sequence of these as the
/// provider streams its response.
///
/// Streaming is modeled as `TextDelta`s interleaved with at most one
/// trailing `Done`. The `Done` carries final accounting that some
/// providers only report at end-of-stream (Anthropic's `message_delta`
/// usage block, for example).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatToken {
    /// An incremental piece of generated text. May be a single token,
    /// a few characters, or a whole word — providers chunk
    /// differently and the orchestrator must not assume a unit.
    TextDelta {
        /// The text fragment to append to the assistant response.
        text: String,
    },
    /// End-of-stream marker. Final usage numbers may not be available
    /// (e.g., on stream error or for providers that don't report them
    /// in-band) so the field is optional.
    Done {
        /// Final token counts, when the provider reports them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_serializes_snake_case() {
        let msg = ChatMessage::system("hello");
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"system\""));
    }

    #[test]
    fn message_round_trips_through_json() {
        let original = ChatMessage::assistant("hi there");
        let json = serde_json::to_string(&original).unwrap();
        let parsed: ChatMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn token_usage_total() {
        let u = TokenUsage {
            input: 100,
            output: 250,
        };
        assert_eq!(u.total(), 350);
    }

    #[test]
    fn cost_addition_accumulates() {
        let mut total = Cost::from_usd(0.10);
        total += Cost::from_usd(0.05);
        let combined = total + Cost::from_usd(0.01);
        assert!((combined.usd - 0.16).abs() < 1e-9);
    }

    #[test]
    fn chat_token_text_delta_round_trip() {
        let t = ChatToken::TextDelta {
            text: "hello".into(),
        };
        let json = serde_json::to_string(&t).unwrap();
        // Tagged enum: kind discriminator should appear.
        assert!(json.contains("\"kind\":\"text_delta\""));
        let parsed: ChatToken = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, t);
    }

    #[test]
    fn chat_token_done_with_and_without_usage() {
        let with = ChatToken::Done {
            usage: Some(TokenUsage {
                input: 5,
                output: 7,
            }),
        };
        let without = ChatToken::Done { usage: None };

        let with_json = serde_json::to_string(&with).unwrap();
        let without_json = serde_json::to_string(&without).unwrap();
        assert!(with_json.contains("\"usage\""));
        // None usage should be skipped, not serialized as "null".
        assert!(!without_json.contains("\"usage\""));

        assert_eq!(serde_json::from_str::<ChatToken>(&with_json).unwrap(), with);
        assert_eq!(
            serde_json::from_str::<ChatToken>(&without_json).unwrap(),
            without
        );
    }
}
