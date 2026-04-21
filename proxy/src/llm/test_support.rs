//! Test-only support: in-memory `LlmProvider` driven by a canned
//! token script. Used by the orchestrator unit tests and the
//! `conversation_api` integration tests.
//!
//! Lives under `#[cfg(test)]` and is `pub(crate)` so multiple test
//! modules can share one definition without duplicating the trait
//! impl. Not built into the production binary.

use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use futures::stream;
use futures::stream::BoxStream;
use parley_core::chat::{ChatMessage, ChatToken, Cost, TokenUsage};
use parley_core::model_config::TokenRates;

use crate::llm::{ChatCompletion, ChatOptions, LlmError, LlmProvider};

/// One scripted output. The mock emits these in order, then a final
/// `Done { usage }` frame.
pub enum MockItem {
    /// Emit a `ChatToken::TextDelta` with this text.
    Text(String),
    /// Emit an error mid-stream.
    Err(LlmError),
}

/// Mock `LlmProvider` driven by a canned token script.
pub struct MockProvider {
    id: String,
    context_window: u32,
    rates: TokenRates,
    script: Arc<StdMutex<Vec<MockItem>>>,
    usage: TokenUsage,
    captured_messages: Arc<StdMutex<Option<Vec<ChatMessage>>>>,
}

impl MockProvider {
    /// Build with a fresh canned script and the usage that will be
    /// reported on the final `Done` frame.
    pub fn new(id: &str, script: Vec<MockItem>, usage: TokenUsage) -> Self {
        Self {
            id: id.into(),
            context_window: 200_000,
            rates: TokenRates {
                input_per_1m: 1.0,
                output_per_1m: 5.0,
            },
            script: Arc::new(StdMutex::new(script)),
            usage,
            captured_messages: Arc::new(StdMutex::new(None)),
        }
    }

    /// Inspect the message history that was passed to the most
    /// recent `stream_chat` call.
    pub fn captured(&self) -> Option<Vec<ChatMessage>> {
        self.captured_messages.lock().unwrap().clone()
    }

    /// Handle to the captured-messages slot, useful when the
    /// `MockProvider` itself has been moved into an `Arc<dyn ...>`.
    pub fn captured_handle(&self) -> Arc<StdMutex<Option<Vec<ChatMessage>>>> {
        self.captured_messages.clone()
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn context_window(&self) -> u32 {
        self.context_window
    }
    fn count_tokens(&self, text: &str) -> u64 {
        text.split_whitespace().count() as u64
    }
    fn cost(&self, usage: TokenUsage) -> Cost {
        Cost::from_usd(
            (usage.input as f64 / 1_000_000.0) * self.rates.input_per_1m
                + (usage.output as f64 / 1_000_000.0) * self.rates.output_per_1m,
        )
    }
    async fn complete(
        &self,
        _messages: &[ChatMessage],
        _opts: &ChatOptions,
    ) -> Result<ChatCompletion, LlmError> {
        unimplemented!("complete is not exercised by the orchestrator slice")
    }
    async fn stream_chat(
        &self,
        messages: &[ChatMessage],
        _opts: &ChatOptions,
    ) -> Result<BoxStream<'static, Result<ChatToken, LlmError>>, LlmError> {
        *self.captured_messages.lock().unwrap() = Some(messages.to_vec());
        let script = std::mem::take(&mut *self.script.lock().unwrap());
        let usage = self.usage;
        let mut items: Vec<Result<ChatToken, LlmError>> = script
            .into_iter()
            .map(|item| match item {
                MockItem::Text(t) => Ok(ChatToken::TextDelta { text: t }),
                MockItem::Err(e) => Err(e),
            })
            .collect();
        items.push(Ok(ChatToken::Done { usage: Some(usage) }));
        Ok(Box::pin(stream::iter(items)))
    }
}
