//! Conversation orchestrator skeleton.
//!
//! The orchestrator is the runtime "harness" that owns conversation
//! state, drives the per-turn state machine, and dispatches user
//! input to the active persona's [`LlmProvider`]. It is **not** part
//! of the audio pipeline — it consumes the pipeline's outputs (and,
//! in a later slice, drives a TTS provider with its outputs).
//!
//! ## Scope of this slice
//!
//! - Single-agent, text-only turns (typed input → LLM → streamed text)
//! - State machine subset: `Idle → Routing → Streaming → Idle` plus
//!   `Failed → Idle`
//! - Persona / model / system-prompt resolution from the registries
//!   built in Phase 3 (`proxy::registry`)
//! - Mid-session persona switching API (no UI yet — spec §6.3)
//! - Per-turn cost computed from the active model's rates
//!
//! ## Deliberately out of scope (later slices)
//!
//! - Audio capture / STT / TTS integration (so the `Capturing`,
//!   `Speaking`, `Paused` states are absent here)
//! - Multi-party / WordGraph AI lane writes
//! - Pause / Stop / Play / barge-in
//! - Context compaction
//! - Persistence (session file format)
//! - Expression-annotation auto-prepend
//! - Retry-on-failure logic
//!
//! Spec references: §3.2, §4 (orchestrator), §5 (state machine),
//! §6.3 (active persona), §10.1 (failure surfacing), §11 (cost).

#![allow(dead_code)] // Skeleton: no production callsite yet. Tests cover the surface.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use futures::stream::BoxStream;
use parley_core::chat::{ChatMessage, ChatToken, Cost, TokenUsage};
use parley_core::conversation::{ConversationSession, TurnId, TurnProvenance};
use parley_core::model_config::{ModelConfig, ModelConfigId};
use parley_core::persona::{Persona, PersonaId, SystemPrompt};
use parley_core::speaker::SpeakerId;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::llm::{ChatOptions, LlmError, LlmProvider};

/// One observable state in the per-turn lifecycle. This is a strict
/// subset of spec §5: the audio-bound states (Capturing,
/// FinalizingStt, Speaking, Paused, Stopped) are deferred until the
/// audio integration slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrchestratorState {
    /// No active turn; ready to accept input.
    Idle,
    /// User turn appended; selecting persona / model / provider.
    Routing,
    /// Provider is streaming the assistant response; tokens flowing.
    Streaming,
    /// Most recent dispatch failed; awaiting caller decision (retry
    /// or skip). The skeleton just transitions back to `Idle`; spec
    /// §10.1 retry/skip UI lands later.
    Failed,
}

/// One observable side-effect of orchestration. The caller (UI,
/// future HTTP endpoint, test harness) consumes a stream of these
/// from [`ConversationOrchestrator::submit_user_turn`].
#[derive(Debug, Clone, PartialEq)]
pub enum OrchestratorEvent {
    /// State machine moved into `state`.
    StateChanged {
        /// New state.
        state: OrchestratorState,
    },
    /// User turn was appended to the session. Carries the assigned
    /// turn id so the caller can correlate.
    UserTurnAppended {
        /// Id of the appended user turn.
        turn_id: TurnId,
    },
    /// Provider emitted an incremental text delta. Concatenating
    /// every `Token` in order rebuilds the assistant response.
    Token {
        /// Text fragment to append to the in-progress assistant
        /// response.
        delta: String,
    },
    /// Assistant turn fully completed and appended to the session.
    /// Carries the final usage + cost so cost meters can update.
    AiTurnAppended {
        /// Id of the appended assistant turn.
        turn_id: TurnId,
        /// Final token accounting reported by the provider.
        usage: TokenUsage,
        /// USD cost for this turn.
        cost: Cost,
    },
    /// Dispatch failed at any point. The orchestrator transitions to
    /// `Failed` then `Idle`; the caller decides whether to retry.
    Failed {
        /// Human-readable error message.
        message: String,
    },
}

/// All ways an orchestrator call can fail before a stream is even
/// returned. Failures *during* streaming surface as
/// [`OrchestratorEvent::Failed`] inside the event stream.
#[derive(Debug, Error)]
pub enum OrchestratorError {
    /// The active persona id doesn't resolve in the persona registry.
    /// Indicates a configuration drift between the session and the
    /// loaded registries.
    #[error("unknown persona: '{0}'")]
    UnknownPersona(PersonaId),
    /// The active persona's heavy-tier model id doesn't resolve in
    /// the model registry.
    #[error("unknown model config: '{0}'")]
    UnknownModelConfig(ModelConfigId),
    /// No `LlmProvider` instance is registered for the active
    /// model. The proxy's startup code is responsible for wiring
    /// this map; a missing entry is a startup-config bug.
    #[error("no provider configured for model '{0}'")]
    NoProvider(ModelConfigId),
    /// The persona's system prompt is `SystemPrompt::File { file }`
    /// but the file could not be read.
    #[error("failed to read system prompt file '{path}': {source}")]
    PromptFile {
        /// Resolved absolute path that was attempted.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// Runtime resources the orchestrator needs to dispatch a turn. Held
/// behind an `Arc` so the (mutable) [`ConversationOrchestrator`] can
/// be cheap to clone for streaming futures.
pub struct OrchestratorContext {
    /// Loaded personas, keyed by id (built by `proxy::registry`).
    pub personas: HashMap<PersonaId, Persona>,
    /// Loaded model configs, keyed by id.
    pub models: HashMap<ModelConfigId, ModelConfig>,
    /// `LlmProvider` instance per model id. The proxy's startup code
    /// builds this — typically one provider per model the user has
    /// API credentials for. A persona referencing a model with no
    /// registered provider yields [`OrchestratorError::NoProvider`].
    pub providers: HashMap<ModelConfigId, Arc<dyn LlmProvider>>,
    /// Directory holding `SystemPrompt::File` referents (typically
    /// `~/.parley/prompts/`).
    pub prompts_dir: PathBuf,
}

/// Trait abstraction over "tell me the current wall-clock millis." A
/// trait so tests can pin time deterministically.
pub trait Clock: Send + Sync {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;
}

/// Real wall-clock implementation.
pub struct SystemClock;
impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Conversation orchestrator. Owns the live session and dispatches
/// user turns to the active persona's provider.
///
/// The session is wrapped in an `Arc<Mutex<_>>` so that the future
/// returned by [`Self::submit_user_turn`] can mutate it across await
/// points without borrowing `&mut self` for the whole stream lifetime.
pub struct ConversationOrchestrator {
    session: Arc<Mutex<ConversationSession>>,
    ctx: Arc<OrchestratorContext>,
    clock: Arc<dyn Clock>,
}

impl ConversationOrchestrator {
    /// Construct an orchestrator around an existing session and
    /// runtime context. Uses the real system clock.
    pub fn new(session: ConversationSession, ctx: OrchestratorContext) -> Self {
        Self::with_clock(session, ctx, Arc::new(SystemClock))
    }

    /// Construct with an injectable clock. Used by tests for
    /// deterministic timestamps.
    pub fn with_clock(
        session: ConversationSession,
        ctx: OrchestratorContext,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            ctx: Arc::new(ctx),
            clock,
        }
    }

    /// Read-only access to the live session — for the caller to
    /// render history, totals, etc. Returns a clone to avoid leaking
    /// the lock guard.
    pub async fn session_snapshot(&self) -> ConversationSession {
        self.session.lock().await.clone()
    }

    /// Switch the active (persona, model). Pure delegation to
    /// [`ConversationSession::switch_persona`] — the orchestrator
    /// itself holds no separate "active" state, so this takes effect
    /// on the next [`Self::submit_user_turn`].
    pub async fn switch_persona(&self, persona_id: PersonaId, model_config_id: ModelConfigId) {
        self.session
            .lock()
            .await
            .switch_persona(persona_id, model_config_id);
    }

    /// Submit a user turn (typed text). Returns a stream of events
    /// that drives the per-turn state machine. The stream is finite
    /// and terminates after either `AiTurnAppended` or `Failed`.
    ///
    /// `speaker_id` must already be registered in the session's
    /// `speakers` table for typical use, but this slice does not
    /// enforce that — registering speakers is the caller's
    /// responsibility until the multi-party slice lands.
    pub async fn submit_user_turn(
        &self,
        speaker_id: SpeakerId,
        content: String,
    ) -> Result<BoxStream<'static, OrchestratorEvent>, OrchestratorError> {
        // 1. Resolve the active persona + model + provider *before*
        //    we mutate the session, so a config error doesn't leave
        //    a dangling user turn with no AI response.
        let (persona, model, provider) = {
            let session = self.session.lock().await;
            let active = session.active().clone();
            let persona = self
                .ctx
                .personas
                .get(&active.persona_id)
                .cloned()
                .ok_or_else(|| OrchestratorError::UnknownPersona(active.persona_id.clone()))?;
            let model = self
                .ctx
                .models
                .get(&active.model_config_id)
                .cloned()
                .ok_or_else(|| {
                    OrchestratorError::UnknownModelConfig(active.model_config_id.clone())
                })?;
            let provider = self
                .ctx
                .providers
                .get(&active.model_config_id)
                .cloned()
                .ok_or_else(|| OrchestratorError::NoProvider(active.model_config_id.clone()))?;
            (persona, model, provider)
        };

        // 2. Resolve the system prompt — inline is free, file
        //    requires I/O. Done before mutating the session for the
        //    same reason as (1).
        let system_text = resolve_system_prompt(&persona.system_prompt, &self.ctx.prompts_dir)?;

        // 3. Append the user turn and snapshot the message history
        //    we'll send.
        let (user_turn_id, history) = {
            let mut session = self.session.lock().await;
            let id = session.append_user_turn(speaker_id, content.clone(), self.clock.now_ms());
            let mut msgs = vec![ChatMessage::system(system_text)];
            msgs.extend(session.to_chat_messages());
            (id, msgs)
        };

        // 4. Build the dispatch future. All subsequent state lives
        //    inside the stream; the caller drives by polling.
        let session = self.session.clone();
        let clock = self.clock.clone();
        let ai_speaker_id = speaker_for_persona(&persona);
        let persona_id = persona.id.clone();
        let model_id = model.id.clone();
        let opts = ChatOptions::default();

        let stream = async_stream::stream! {
            yield OrchestratorEvent::UserTurnAppended { turn_id: user_turn_id.clone() };
            yield OrchestratorEvent::StateChanged { state: OrchestratorState::Routing };
            yield OrchestratorEvent::StateChanged { state: OrchestratorState::Streaming };

            let token_stream = match provider.stream_chat(&history, &opts).await {
                Ok(s) => s,
                Err(e) => {
                    yield OrchestratorEvent::Failed { message: format_llm_error(&e) };
                    yield OrchestratorEvent::StateChanged { state: OrchestratorState::Failed };
                    yield OrchestratorEvent::StateChanged { state: OrchestratorState::Idle };
                    return;
                }
            };

            let mut accumulated = String::new();
            let mut final_usage = TokenUsage::default();
            let mut errored = false;

            futures::pin_mut!(token_stream);
            while let Some(item) = token_stream.next().await {
                match item {
                    Ok(ChatToken::TextDelta { text }) => {
                        accumulated.push_str(&text);
                        yield OrchestratorEvent::Token { delta: text };
                    }
                    Ok(ChatToken::Done { usage }) => {
                        if let Some(u) = usage { final_usage = u; }
                    }
                    Err(e) => {
                        yield OrchestratorEvent::Failed { message: format_llm_error(&e) };
                        errored = true;
                        break;
                    }
                }
            }

            if errored {
                yield OrchestratorEvent::StateChanged { state: OrchestratorState::Failed };
                yield OrchestratorEvent::StateChanged { state: OrchestratorState::Idle };
                return;
            }

            let cost = provider.cost(final_usage);
            let provenance = TurnProvenance {
                persona_id: persona_id.clone(),
                model_config_id: model_id.clone(),
                usage: final_usage,
                cost,
            };

            let ai_turn_id = {
                let mut session = session.lock().await;
                session.append_ai_turn(
                    ai_speaker_id,
                    accumulated,
                    clock.now_ms(),
                    provenance,
                )
            };

            yield OrchestratorEvent::AiTurnAppended {
                turn_id: ai_turn_id,
                usage: final_usage,
                cost,
            };
            yield OrchestratorEvent::StateChanged { state: OrchestratorState::Idle };
        };

        Ok(Box::pin(stream))
    }
}

/// Convention: an AI agent's speaker id is `"ai-<persona_id>"`. The
/// orchestrator does not enforce that speakers be pre-registered in
/// the session; multi-speaker registration lands in a later slice.
fn speaker_for_persona(persona: &Persona) -> SpeakerId {
    format!("ai-{}", persona.id)
}

/// Resolve a [`SystemPrompt`] into its full text. Inline returns its
/// own text; File reads from `prompts_dir`, accepting both `"name"`
/// and `"name.md"` forms (matching `proxy::registry`'s validation
/// behavior).
fn resolve_system_prompt(
    prompt: &SystemPrompt,
    prompts_dir: &std::path::Path,
) -> Result<String, OrchestratorError> {
    match prompt {
        SystemPrompt::Inline { text } => Ok(text.clone()),
        SystemPrompt::File { file } => {
            let path = if file.contains('.') {
                prompts_dir.join(file)
            } else {
                prompts_dir.join(format!("{file}.md"))
            };
            std::fs::read_to_string(&path)
                .map_err(|source| OrchestratorError::PromptFile { path, source })
        }
    }
}

/// One-line formatter for an [`LlmError`] so it surfaces cleanly in
/// the event stream.
fn format_llm_error(e: &LlmError) -> String {
    e.to_string()
}

// async-stream provides the `stream! { ... }` macro used in
// `submit_user_turn`; it does not need a `use` import here because
// the macro is referenced through its absolute path.

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::stream;
    use parley_core::chat::ChatToken;
    use parley_core::model_config::{LlmProviderTag, TokenRates};
    use parley_core::persona::{
        PersonaContextSettings, PersonaTier, PersonaTiers, PersonaTtsSettings, SystemPrompt,
    };
    use parley_core::speaker::Speaker;
    use std::sync::Mutex as StdMutex;

    /// Mock `LlmProvider` driven by a canned token script. Emits each
    /// scripted item in order, then a `Done` with the configured
    /// usage. Behaves like a well-formed Anthropic-style stream
    /// without any HTTP at all.
    struct MockProvider {
        id: String,
        context_window: u32,
        rates: TokenRates,
        script: Arc<StdMutex<Vec<MockItem>>>,
        usage: TokenUsage,
        captured_messages: Arc<StdMutex<Option<Vec<ChatMessage>>>>,
    }

    enum MockItem {
        Text(String),
        Err(LlmError),
    }

    impl MockProvider {
        fn new(id: &str, script: Vec<MockItem>, usage: TokenUsage) -> Self {
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

        fn captured(&self) -> Option<Vec<ChatMessage>> {
            self.captured_messages.lock().unwrap().clone()
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
        ) -> Result<crate::llm::ChatCompletion, LlmError> {
            unimplemented!("complete is not exercised by the orchestrator skeleton")
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

    struct FakeClock(u64);
    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0
        }
    }

    fn sample_persona(id: &str, model_id: &str, prompt: &str) -> Persona {
        Persona {
            id: id.into(),
            name: id.into(),
            description: String::new(),
            system_prompt: SystemPrompt::Inline {
                text: prompt.into(),
            },
            tiers: PersonaTiers {
                heavy: PersonaTier {
                    model_config: model_id.into(),
                    voice: "elevenlabs:rachel".into(),
                    tts_model: "eleven_v3".into(),
                    narration_style: None,
                },
                fast: None,
            },
            tts: PersonaTtsSettings::default(),
            context: PersonaContextSettings::default(),
        }
    }

    fn sample_model(id: &str) -> ModelConfig {
        ModelConfig {
            id: id.into(),
            provider: LlmProviderTag::Anthropic,
            model_name: "claude-test".into(),
            context_window: 200_000,
            rates: TokenRates {
                input_per_1m: 1.0,
                output_per_1m: 5.0,
            },
            options: serde_json::Value::Null,
        }
    }

    fn build(
        persona: Persona,
        model: ModelConfig,
        provider: Arc<dyn LlmProvider>,
    ) -> ConversationOrchestrator {
        let session = ConversationSession::new(
            "sess-1",
            Speaker::ai_agent(format!("ai-{}", persona.id), &persona.name),
            persona.id.clone(),
            model.id.clone(),
        );
        let ctx = OrchestratorContext {
            personas: [(persona.id.clone(), persona)].into(),
            models: [(model.id.clone(), model.clone())].into(),
            providers: [(model.id.clone(), provider)].into(),
            prompts_dir: PathBuf::from("/nonexistent"),
        };
        ConversationOrchestrator::with_clock(session, ctx, Arc::new(FakeClock(1_000)))
    }

    async fn drain(o: &ConversationOrchestrator, text: &str) -> Vec<OrchestratorEvent> {
        let mut s = match o.submit_user_turn("gavin".into(), text.into()).await {
            Ok(s) => s,
            Err(e) => panic!("submit failed: {e}"),
        };
        let mut out = Vec::new();
        while let Some(ev) = s.next().await {
            out.push(ev);
        }
        out
    }

    fn states(events: &[OrchestratorEvent]) -> Vec<OrchestratorState> {
        events
            .iter()
            .filter_map(|e| {
                if let OrchestratorEvent::StateChanged { state } = e {
                    Some(*state)
                } else {
                    None
                }
            })
            .collect()
    }

    #[tokio::test]
    async fn happy_path_emits_expected_event_sequence() {
        let provider = Arc::new(MockProvider::new(
            "p",
            vec![
                MockItem::Text("Hello, ".into()),
                MockItem::Text("world.".into()),
            ],
            TokenUsage {
                input: 10,
                output: 4,
            },
        ));
        let o = build(
            sample_persona("scholar", "m1", "be helpful"),
            sample_model("m1"),
            provider,
        );
        let events = drain(&o, "hi").await;

        assert_eq!(
            states(&events),
            vec![
                OrchestratorState::Routing,
                OrchestratorState::Streaming,
                OrchestratorState::Idle,
            ]
        );
        // First non-state event is the user-turn append.
        assert!(matches!(
            events.first(),
            Some(OrchestratorEvent::UserTurnAppended { .. })
        ));
        // Token deltas in order.
        let tokens: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                OrchestratorEvent::Token { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tokens, vec!["Hello, ", "world."]);
        // Final AI turn carries usage + cost.
        let ai = events
            .iter()
            .find_map(|e| match e {
                OrchestratorEvent::AiTurnAppended { usage, cost, .. } => Some((*usage, *cost)),
                _ => None,
            })
            .expect("AiTurnAppended");
        assert_eq!(ai.0.output, 4);
        // 10 input @ $1/M + 4 output @ $5/M = $0.00001 + $0.00002 = $0.00003
        assert!((ai.1.usd - 0.00003).abs() < 1e-9);
    }

    #[tokio::test]
    async fn ai_turn_is_persisted_with_provenance() {
        let provider = Arc::new(MockProvider::new(
            "p",
            vec![MockItem::Text("ok".into())],
            TokenUsage {
                input: 1,
                output: 1,
            },
        ));
        let o = build(
            sample_persona("scholar", "m1", "be helpful"),
            sample_model("m1"),
            provider,
        );
        let _ = drain(&o, "hi").await;
        let snap = o.session_snapshot().await;
        assert_eq!(snap.turns.len(), 2);
        let prov = snap.turns[1].provenance.as_ref().unwrap();
        assert_eq!(prov.persona_id, "scholar");
        assert_eq!(prov.model_config_id, "m1");
    }

    #[tokio::test]
    async fn provider_error_during_stream_surfaces_as_failed() {
        let provider = Arc::new(MockProvider::new(
            "p",
            vec![
                MockItem::Text("partial".into()),
                MockItem::Err(LlmError::Other("boom".into())),
            ],
            TokenUsage::default(),
        ));
        let o = build(
            sample_persona("scholar", "m1", "be helpful"),
            sample_model("m1"),
            provider,
        );
        let events = drain(&o, "hi").await;

        assert!(events.iter().any(
            |e| matches!(e, OrchestratorEvent::Failed { message } if message.contains("boom"))
        ));
        // Should pass through Failed → Idle.
        let s = states(&events);
        assert!(s.contains(&OrchestratorState::Failed));
        assert_eq!(s.last().copied(), Some(OrchestratorState::Idle));
        // No AI turn was appended on failure.
        let snap = o.session_snapshot().await;
        assert_eq!(snap.turns.len(), 1);
        assert!(snap.turns[0].provenance.is_none());
    }

    #[tokio::test]
    async fn unknown_persona_in_session_is_an_error() {
        // Build a session whose active persona id is not in the
        // registry (drift).
        let provider: Arc<dyn LlmProvider> =
            Arc::new(MockProvider::new("p", vec![], TokenUsage::default()));
        let model = sample_model("m1");
        let session = ConversationSession::new(
            "sess",
            Speaker::ai_agent("ai-ghost", "Ghost"),
            "ghost".into(),
            "m1".into(),
        );
        let ctx = OrchestratorContext {
            personas: HashMap::new(), // no personas
            models: [("m1".into(), model.clone())].into(),
            providers: [("m1".into(), provider)].into(),
            prompts_dir: PathBuf::from("/x"),
        };
        let o = ConversationOrchestrator::new(session, ctx);
        let err = match o.submit_user_turn("gavin".into(), "hi".into()).await {
            Err(e) => e,
            Ok(_) => panic!("expected UnknownPersona"),
        };
        assert!(matches!(err, OrchestratorError::UnknownPersona(_)));
        // No turn should have been appended on early-out.
        assert_eq!(o.session_snapshot().await.turns.len(), 0);
    }

    #[tokio::test]
    async fn missing_provider_reports_no_provider_error() {
        let model = sample_model("m1");
        let persona = sample_persona("scholar", "m1", "x");
        let session = ConversationSession::new(
            "s",
            Speaker::ai_agent("ai-scholar", "Scholar"),
            persona.id.clone(),
            model.id.clone(),
        );
        let ctx = OrchestratorContext {
            personas: [(persona.id.clone(), persona)].into(),
            models: [(model.id.clone(), model)].into(),
            providers: HashMap::new(), // no provider
            prompts_dir: PathBuf::from("/x"),
        };
        let o = ConversationOrchestrator::new(session, ctx);
        let err = match o.submit_user_turn("gavin".into(), "hi".into()).await {
            Err(e) => e,
            Ok(_) => panic!("expected NoProvider"),
        };
        assert!(matches!(err, OrchestratorError::NoProvider(_)));
    }

    #[tokio::test]
    async fn switch_persona_takes_effect_on_next_turn() {
        let p1 = Arc::new(MockProvider::new(
            "p1",
            vec![MockItem::Text("from-1".into())],
            TokenUsage::default(),
        ));
        let p2 = Arc::new(MockProvider::new(
            "p2",
            vec![MockItem::Text("from-2".into())],
            TokenUsage::default(),
        ));
        let persona1 = sample_persona("p1", "m1", "one");
        let persona2 = sample_persona("p2", "m2", "two");
        let model1 = sample_model("m1");
        let model2 = sample_model("m2");
        let session = ConversationSession::new(
            "s",
            Speaker::ai_agent("ai-p1", "P1"),
            persona1.id.clone(),
            model1.id.clone(),
        );
        let ctx = OrchestratorContext {
            personas: [
                (persona1.id.clone(), persona1.clone()),
                (persona2.id.clone(), persona2.clone()),
            ]
            .into(),
            models: [
                (model1.id.clone(), model1.clone()),
                (model2.id.clone(), model2.clone()),
            ]
            .into(),
            providers: [
                (model1.id.clone(), p1 as Arc<dyn LlmProvider>),
                (model2.id.clone(), p2 as Arc<dyn LlmProvider>),
            ]
            .into(),
            prompts_dir: PathBuf::from("/x"),
        };
        let o = ConversationOrchestrator::with_clock(session, ctx, Arc::new(FakeClock(1)));

        let _ = drain(&o, "hi").await;
        o.switch_persona("p2".into(), "m2".into()).await;
        let _ = drain(&o, "hi again").await;

        let snap = o.session_snapshot().await;
        assert_eq!(snap.persona_history.len(), 2);
        assert_eq!(snap.persona_history[1].persona_id, "p2");
        // First AI turn from persona1, second from persona2.
        let provs: Vec<&str> = snap
            .turns
            .iter()
            .filter_map(|t| t.provenance.as_ref().map(|p| p.persona_id.as_str()))
            .collect();
        assert_eq!(provs, vec!["p1", "p2"]);
    }

    #[tokio::test]
    async fn system_prompt_inline_is_prepended_to_history() {
        let provider = Arc::new(MockProvider::new(
            "p",
            vec![MockItem::Text("ok".into())],
            TokenUsage::default(),
        ));
        let captured_handle = provider.captured_messages.clone();
        let o = build(
            sample_persona("scholar", "m1", "BE BRIEF"),
            sample_model("m1"),
            provider,
        );
        let _ = drain(&o, "hi").await;
        let captured = captured_handle.lock().unwrap().clone().expect("captured");
        // First message must be the system prompt.
        assert_eq!(captured[0].role, parley_core::chat::ChatRole::System);
        assert_eq!(captured[0].content, "BE BRIEF");
        // Followed by the user turn.
        assert_eq!(captured[1].role, parley_core::chat::ChatRole::User);
        assert_eq!(captured[1].content, "hi");
    }

    #[tokio::test]
    async fn system_prompt_file_is_read_from_prompts_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prompt_path = tmp.path().join("scholar.md");
        std::fs::write(&prompt_path, "loaded from disk").unwrap();

        let provider = Arc::new(MockProvider::new(
            "p",
            vec![MockItem::Text("ok".into())],
            TokenUsage::default(),
        ));
        let captured_handle = provider.captured_messages.clone();
        let mut persona = sample_persona("scholar", "m1", "");
        persona.system_prompt = SystemPrompt::File {
            file: "scholar".into(),
        };
        let model = sample_model("m1");
        let session = ConversationSession::new(
            "s",
            Speaker::ai_agent("ai-scholar", "Scholar"),
            persona.id.clone(),
            model.id.clone(),
        );
        let ctx = OrchestratorContext {
            personas: [(persona.id.clone(), persona)].into(),
            models: [(model.id.clone(), model)].into(),
            providers: [(("m1").into(), provider as Arc<dyn LlmProvider>)].into(),
            prompts_dir: tmp.path().to_path_buf(),
        };
        let o = ConversationOrchestrator::with_clock(session, ctx, Arc::new(FakeClock(1)));
        let _ = drain(&o, "hi").await;
        let captured = captured_handle.lock().unwrap().clone().expect("captured");
        assert_eq!(captured[0].content, "loaded from disk");
    }

    #[tokio::test]
    async fn system_prompt_missing_file_is_an_error() {
        let provider: Arc<dyn LlmProvider> =
            Arc::new(MockProvider::new("p", vec![], TokenUsage::default()));
        let mut persona = sample_persona("scholar", "m1", "");
        persona.system_prompt = SystemPrompt::File {
            file: "nope".into(),
        };
        let model = sample_model("m1");
        let session = ConversationSession::new(
            "s",
            Speaker::ai_agent("ai-scholar", "Scholar"),
            persona.id.clone(),
            model.id.clone(),
        );
        let ctx = OrchestratorContext {
            personas: [(persona.id.clone(), persona)].into(),
            models: [(model.id.clone(), model)].into(),
            providers: [("m1".into(), provider)].into(),
            prompts_dir: PathBuf::from("/definitely/not/here"),
        };
        let o = ConversationOrchestrator::new(session, ctx);
        let err = match o.submit_user_turn("gavin".into(), "hi".into()).await {
            Err(e) => e,
            Ok(_) => panic!("expected PromptFile"),
        };
        assert!(matches!(err, OrchestratorError::PromptFile { .. }));
    }
}
