//! Anthropic Messages API implementation of [`LlmProvider`].
//!
//! Wraps the Anthropic `/v1/messages` HTTP endpoint, supporting both
//! one-shot completion and SSE streaming.
//!
//! ## Notable shape decisions
//!
//! - **System prompt extraction.** Anthropic puts the system prompt in
//!   a top-level `system` field, not in the `messages` array. We pull
//!   the first contiguous run of [`ChatRole::System`] messages off the
//!   front and concatenate them with `\n\n` before sending.
//! - **Stream → `ChatToken` mapping.** We emit a `ChatToken::TextDelta`
//!   for each `content_block_delta` of type `text_delta`, then exactly
//!   one `ChatToken::Done` when the `message_stop` event arrives. The
//!   `message_delta` event carries final `output_tokens`; we cache it
//!   and merge with the `message_start` `input_tokens` at `Done`.
//! - **Tokenizer.** Anthropic does not expose an offline tokenizer, so
//!   `count_tokens` falls back to the word-count proxy described in
//!   spec §9.2 (~0.75 tokens per word for English; we use 1.3 words
//!   per token as a slight over-estimate for safety).
//!
//! Spec reference: `docs/conversation-mode-spec.md` §12.

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use parley_core::chat::{ChatMessage, ChatRole, ChatToken, Cost, TokenUsage};
use parley_core::model_config::TokenRates;
use serde_json::{Value, json};

use super::sse::SseDecoder;
use super::{ChatCompletion, ChatOptions, LlmError, LlmProvider, LlmResult};

/// Default Anthropic Messages endpoint. Constructor takes an override
/// so tests can point at a mock server (not used yet, but the seam is
/// there).
pub const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";

/// Anthropic version header value. Pinned here so changing it touches
/// one place.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Concrete Anthropic implementation of [`LlmProvider`]. One instance
/// per (model, key) pair; cheap to construct, holds a shared reqwest
/// client.
#[derive(Clone)]
pub struct AnthropicLlm {
    /// Stable id like `"anthropic:claude-haiku-4-5-20251001"`. Used
    /// for logging.
    id: String,
    /// Anthropic model identifier, e.g. `"claude-haiku-4-5-20251001"`.
    model: String,
    /// API key. Held in memory only — never logged.
    api_key: String,
    /// Context window in tokens (from the model config).
    context_window: u32,
    /// Per-million-token cost rates.
    rates: TokenRates,
    /// HTTP endpoint. Defaults to [`ANTHROPIC_MESSAGES_URL`]; override
    /// via [`AnthropicLlm::with_endpoint`] for tests.
    endpoint: String,
    /// Shared HTTP client. Cloning is cheap (`Arc` inside).
    client: reqwest::Client,
}

impl AnthropicLlm {
    /// Build a new provider. `id` is whatever stable identifier the
    /// caller wants surfaced in logs (typically
    /// `"anthropic:<model>"`). `api_key` is the raw secret.
    pub fn new(
        id: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
        context_window: u32,
        rates: TokenRates,
        client: reqwest::Client,
    ) -> Self {
        Self {
            id: id.into(),
            model: model.into(),
            api_key: api_key.into(),
            context_window,
            rates,
            endpoint: ANTHROPIC_MESSAGES_URL.to_string(),
            client,
        }
    }

    /// Override the HTTP endpoint. Lets tests point at a local mock
    /// server.
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Pull the leading run of system messages off the front and
    /// return `(system_text, remaining_messages)`. Anthropic wants
    /// system separately from the conversation array.
    fn split_system(messages: &[ChatMessage]) -> (Option<String>, &[ChatMessage]) {
        let split = messages
            .iter()
            .position(|m| m.role != ChatRole::System)
            .unwrap_or(messages.len());
        if split == 0 {
            (None, messages)
        } else {
            let system = messages[..split]
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            (Some(system), &messages[split..])
        }
    }

    /// Map a `ChatRole` to Anthropic's wire string. `System` is
    /// already extracted by `split_system`, so this only handles the
    /// remaining two roles.
    fn role_str(role: ChatRole) -> &'static str {
        match role {
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
            // System should never reach here — split_system removes
            // them — but if it does, treat as user to keep the
            // request well-formed.
            ChatRole::System => "user",
        }
    }

    /// Assemble the JSON request body. Shared between `complete` and
    /// `stream_chat`; only the `stream` flag differs.
    fn build_payload(&self, messages: &[ChatMessage], opts: &ChatOptions, stream: bool) -> Value {
        let (system, convo) = Self::split_system(messages);
        let messages_json: Vec<Value> = convo
            .iter()
            .map(|m| {
                json!({
                    "role": Self::role_str(m.role),
                    "content": m.content,
                })
            })
            .collect();

        // Anthropic requires max_tokens. Default to a reasonable cap
        // when the caller didn't specify one.
        let max_tokens = opts.max_tokens.unwrap_or(4096);

        let mut payload = json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "messages": messages_json,
            "stream": stream,
        });

        if let Some(sys) = system {
            payload["system"] = Value::String(sys);
        }
        if let Some(t) = opts.temperature {
            payload["temperature"] = json!(t);
        }
        if !opts.stop.is_empty() {
            payload["stop_sequences"] = json!(opts.stop);
        }

        // Merge provider extensions last so they can override anything
        // we set above (deliberate — caller knows what they're doing).
        if let Value::Object(map) = &opts.provider_extensions {
            for (k, v) in map {
                payload[k] = v.clone();
            }
        }

        payload
    }

    /// Map an HTTP response status into an [`LlmError`] kind. 401/403
    /// becomes `Auth`; everything else non-2xx becomes `Http`.
    fn classify_http(status: reqwest::StatusCode, body: String) -> LlmError {
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            LlmError::Auth(body)
        } else {
            LlmError::Http {
                status: status.as_u16(),
                body,
            }
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicLlm {
    fn id(&self) -> &str {
        &self.id
    }

    fn context_window(&self) -> u32 {
        self.context_window
    }

    fn count_tokens(&self, text: &str) -> u64 {
        // Word-count proxy per spec §9.2. Slightly conservative
        // (1.3 tokens per word) so we trigger compaction a bit early
        // rather than blow through a context limit.
        let words = text.split_whitespace().count() as f64;
        (words * 1.3).ceil() as u64
    }

    fn cost(&self, usage: TokenUsage) -> Cost {
        let usd = (usage.input as f64 / 1_000_000.0) * self.rates.input_per_1m
            + (usage.output as f64 / 1_000_000.0) * self.rates.output_per_1m;
        Cost::from_usd(usd)
    }

    async fn complete(
        &self,
        messages: &[ChatMessage],
        opts: &ChatOptions,
    ) -> LlmResult<ChatCompletion> {
        let payload = self.build_payload(messages, opts, false);
        let resp = self
            .client
            .post(&self.endpoint)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;

        if !status.is_success() {
            return Err(Self::classify_http(status, body));
        }

        let json: Value = serde_json::from_str(&body)
            .map_err(|e| LlmError::BadResponse(format!("non-JSON body: {e}")))?;

        // Concatenate every text content block. Anthropic returns an
        // array; non-text blocks (tool_use, etc.) are ignored for now.
        let text = json
            .get("content")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|block| {
                        if block.get("type").and_then(Value::as_str) == Some("text") {
                            block.get("text").and_then(Value::as_str)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("")
            })
            .ok_or_else(|| LlmError::BadResponse("missing content array".into()))?;

        let usage = TokenUsage {
            input: json["usage"]["input_tokens"].as_u64().unwrap_or(0),
            output: json["usage"]["output_tokens"].as_u64().unwrap_or(0),
        };

        Ok(ChatCompletion { text, usage })
    }

    async fn stream_chat(
        &self,
        messages: &[ChatMessage],
        opts: &ChatOptions,
    ) -> LlmResult<BoxStream<'static, LlmResult<ChatToken>>> {
        let payload = self.build_payload(messages, opts, true);
        let resp = self
            .client
            .post(&self.endpoint)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&payload)
            .send()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::classify_http(status, body));
        }

        // Take the byte stream and lift it through the SSE decoder
        // and the Anthropic frame interpreter.
        let bytes = resp.bytes_stream();
        let mut decoder = SseDecoder::new();
        let mut input_tokens: Option<u64> = None;
        let mut output_tokens: Option<u64> = None;
        let mut done_emitted = false;

        let stream = bytes.flat_map(move |chunk_res| {
            let mut emit: Vec<LlmResult<ChatToken>> = Vec::new();
            match chunk_res {
                Ok(bytes) => decoder.push(&bytes),
                Err(e) => {
                    emit.push(Err(LlmError::Transport(e.to_string())));
                    return stream::iter(emit);
                }
            }
            while let Some(frame) = decoder.next_frame() {
                match interpret_anthropic_frame(&frame) {
                    Ok(Some(StreamEvent::TextDelta(text))) => {
                        emit.push(Ok(ChatToken::TextDelta { text }));
                    }
                    Ok(Some(StreamEvent::InputTokens(n))) => {
                        input_tokens = Some(n);
                    }
                    Ok(Some(StreamEvent::OutputTokens(n))) => {
                        output_tokens = Some(n);
                    }
                    Ok(Some(StreamEvent::Stop)) => {
                        let usage = match (input_tokens, output_tokens) {
                            (Some(i), Some(o)) => Some(TokenUsage {
                                input: i,
                                output: o,
                            }),
                            (Some(i), None) => Some(TokenUsage {
                                input: i,
                                output: 0,
                            }),
                            (None, Some(o)) => Some(TokenUsage {
                                input: 0,
                                output: o,
                            }),
                            (None, None) => None,
                        };
                        emit.push(Ok(ChatToken::Done { usage }));
                        done_emitted = true;
                    }
                    Ok(Some(StreamEvent::Error(msg))) => {
                        emit.push(Err(LlmError::Other(msg)));
                    }
                    Ok(None) => {} // ignored event (e.g. ping)
                    Err(e) => emit.push(Err(e)),
                }
            }
            // Underscore to suppress unused warnings if Done was never
            // seen — caller can still detect end-of-stream via the
            // futures Stream contract.
            let _ = done_emitted;
            stream::iter(emit)
        });

        Ok(Box::pin(stream))
    }
}

/// Decoded meaning of one Anthropic SSE frame.
#[derive(Debug, PartialEq, Eq)]
enum StreamEvent {
    /// A `content_block_delta` carrying generated text.
    TextDelta(String),
    /// `message_start` carrying initial `input_tokens` count.
    InputTokens(u64),
    /// `message_delta` carrying final `output_tokens` count.
    OutputTokens(u64),
    /// `message_stop` — end of stream.
    Stop,
    /// An `error` event from the provider mid-stream.
    Error(String),
}

/// Pure interpretation of one SSE frame. Returns `Ok(None)` for events
/// we deliberately ignore (e.g. `ping`, `content_block_start`,
/// `content_block_stop`). Returns `Err` only when the frame's own JSON
/// is malformed; structural surprises (unknown event names, missing
/// optional fields) become `Ok(None)` and are silently passed by.
fn interpret_anthropic_frame(frame: &super::sse::SseFrame) -> LlmResult<Option<StreamEvent>> {
    let Some(event) = &frame.event else {
        // No event name — Anthropic always sets one, so an unnamed
        // frame is unusual but not a hard error.
        return Ok(None);
    };
    let parse_json = || -> LlmResult<Value> {
        serde_json::from_str(&frame.data)
            .map_err(|e| LlmError::BadResponse(format!("invalid JSON in {event} frame: {e}")))
    };
    match event.as_str() {
        "message_start" => {
            let json = parse_json()?;
            let n = json["message"]["usage"]["input_tokens"]
                .as_u64()
                .unwrap_or(0);
            Ok(Some(StreamEvent::InputTokens(n)))
        }
        "content_block_delta" => {
            let json = parse_json()?;
            // Anthropic uses {"delta":{"type":"text_delta","text":"..."}}
            // for plain text. Other delta types (input_json_delta for
            // tool calls) are ignored.
            let delta = &json["delta"];
            if delta.get("type").and_then(Value::as_str) == Some("text_delta") {
                let text = delta
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                Ok(Some(StreamEvent::TextDelta(text)))
            } else {
                Ok(None)
            }
        }
        "message_delta" => {
            let json = parse_json()?;
            let n = json["usage"]["output_tokens"].as_u64().unwrap_or(0);
            Ok(Some(StreamEvent::OutputTokens(n)))
        }
        "message_stop" => Ok(Some(StreamEvent::Stop)),
        "error" => {
            let json = parse_json()?;
            let msg = json["error"]["message"]
                .as_str()
                .unwrap_or("unknown error")
                .to_string();
            Ok(Some(StreamEvent::Error(msg)))
        }
        // ping, content_block_start, content_block_stop, etc.
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::sse::SseFrame;

    fn frame(event: &str, data: &str) -> SseFrame {
        SseFrame {
            event: Some(event.into()),
            data: data.into(),
        }
    }

    fn rates() -> TokenRates {
        TokenRates {
            input_per_1m: 3.0,
            output_per_1m: 15.0,
        }
    }

    fn provider() -> AnthropicLlm {
        AnthropicLlm::new(
            "anthropic:test",
            "claude-test",
            "sk-test",
            200_000,
            rates(),
            reqwest::Client::new(),
        )
    }

    // ── frame interpretation ──────────────────────────────────────

    #[test]
    fn interprets_message_start_input_tokens() {
        let f = frame(
            "message_start",
            r#"{"type":"message_start","message":{"id":"m","usage":{"input_tokens":42,"output_tokens":1}}}"#,
        );
        assert_eq!(
            interpret_anthropic_frame(&f).unwrap(),
            Some(StreamEvent::InputTokens(42))
        );
    }

    #[test]
    fn interprets_text_delta() {
        let f = frame(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
        );
        assert_eq!(
            interpret_anthropic_frame(&f).unwrap(),
            Some(StreamEvent::TextDelta("Hello".into()))
        );
    }

    #[test]
    fn ignores_non_text_delta_kinds() {
        let f = frame(
            "content_block_delta",
            r#"{"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        );
        assert_eq!(interpret_anthropic_frame(&f).unwrap(), None);
    }

    #[test]
    fn interprets_message_delta_output_tokens() {
        let f = frame(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":17}}"#,
        );
        assert_eq!(
            interpret_anthropic_frame(&f).unwrap(),
            Some(StreamEvent::OutputTokens(17))
        );
    }

    #[test]
    fn interprets_message_stop() {
        let f = frame("message_stop", r#"{"type":"message_stop"}"#);
        assert_eq!(
            interpret_anthropic_frame(&f).unwrap(),
            Some(StreamEvent::Stop)
        );
    }

    #[test]
    fn interprets_error_event() {
        let f = frame(
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"please retry"}}"#,
        );
        assert_eq!(
            interpret_anthropic_frame(&f).unwrap(),
            Some(StreamEvent::Error("please retry".into()))
        );
    }

    #[test]
    fn unknown_event_is_ignored_not_errored() {
        let f = frame("ping", r#"{}"#);
        assert_eq!(interpret_anthropic_frame(&f).unwrap(), None);
    }

    #[test]
    fn malformed_json_in_known_event_errors() {
        let f = frame("message_start", "not json");
        assert!(matches!(
            interpret_anthropic_frame(&f),
            Err(LlmError::BadResponse(_))
        ));
    }

    // ── system extraction ─────────────────────────────────────────

    #[test]
    fn split_system_pulls_leading_system_messages() {
        let msgs = vec![
            ChatMessage::system("you are a tester"),
            ChatMessage::system("be terse"),
            ChatMessage::user("hi"),
            ChatMessage::assistant("hello"),
        ];
        let (sys, rest) = AnthropicLlm::split_system(&msgs);
        assert_eq!(sys.as_deref(), Some("you are a tester\n\nbe terse"));
        assert_eq!(rest.len(), 2);
        assert_eq!(rest[0].role, ChatRole::User);
    }

    #[test]
    fn split_system_no_system_returns_none() {
        let msgs = vec![ChatMessage::user("hi")];
        let (sys, rest) = AnthropicLlm::split_system(&msgs);
        assert!(sys.is_none());
        assert_eq!(rest.len(), 1);
    }

    // ── payload shape ─────────────────────────────────────────────

    #[test]
    fn payload_includes_system_max_tokens_and_messages() {
        let p = provider();
        let msgs = vec![ChatMessage::system("be brief"), ChatMessage::user("hello")];
        let opts = ChatOptions {
            temperature: Some(0.5),
            max_tokens: Some(123),
            stop: vec!["END".into()],
            ..Default::default()
        };
        let payload = p.build_payload(&msgs, &opts, true);
        assert_eq!(payload["model"], "claude-test");
        assert_eq!(payload["max_tokens"], 123);
        assert_eq!(payload["system"], "be brief");
        assert_eq!(payload["temperature"], 0.5);
        assert_eq!(payload["stream"], true);
        assert_eq!(payload["stop_sequences"], json!(["END"]));
        assert_eq!(payload["messages"][0]["role"], "user");
        assert_eq!(payload["messages"][0]["content"], "hello");
    }

    #[test]
    fn payload_omits_system_when_no_system_messages() {
        let p = provider();
        let msgs = vec![ChatMessage::user("hi")];
        let payload = p.build_payload(&msgs, &ChatOptions::default(), false);
        assert!(payload.get("system").is_none());
        assert_eq!(payload["max_tokens"], 4096);
    }

    #[test]
    fn provider_extensions_merge_into_payload() {
        let p = provider();
        let opts = ChatOptions {
            provider_extensions: json!({
                "thinking": { "type": "enabled", "budget_tokens": 8000 }
            }),
            ..Default::default()
        };
        let payload = p.build_payload(&[ChatMessage::user("hi")], &opts, false);
        assert_eq!(payload["thinking"]["budget_tokens"], 8000);
    }

    // ── cost + token-count ────────────────────────────────────────

    #[test]
    fn cost_is_rates_times_usage() {
        let p = provider();
        // 1M input @ $3, 1M output @ $15 → $18 total.
        let c = p.cost(TokenUsage {
            input: 1_000_000,
            output: 1_000_000,
        });
        assert!((c.usd - 18.0).abs() < 1e-9);
    }

    #[test]
    fn count_tokens_word_count_proxy() {
        let p = provider();
        // 10 words * 1.3 = 13 (ceil)
        assert_eq!(
            p.count_tokens("one two three four five six seven eight nine ten"),
            13
        );
    }

    #[test]
    fn count_tokens_zero_for_empty() {
        let p = provider();
        assert_eq!(p.count_tokens(""), 0);
    }

    // ── trait identity ────────────────────────────────────────────

    #[test]
    fn id_and_context_window_pass_through() {
        let p = provider();
        assert_eq!(p.id(), "anthropic:test");
        assert_eq!(p.context_window(), 200_000);
    }
}
