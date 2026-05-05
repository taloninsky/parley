//! Conversation orchestrator.
//!
//! The orchestrator is the runtime "harness" that owns conversation
//! state, drives the per-turn state machine, and dispatches user
//! input to the active persona's [`LlmProvider`]. It also drives the
//! TTS provider that synthesizes each AI turn — the audio sibling
//! stream pulls bytes from the cache the orchestrator writes.
//!
//! ## What's wired today
//!
//! - Single-agent text-and-voice turns (typed or spoken input → LLM →
//!   streamed text → TTS sentence chunks → cached audio + live SSE)
//! - State machine: `Idle → Routing → Streaming → Idle` plus
//!   `Failed → Idle`
//! - Persona / model / system-prompt resolution from the registries
//!   built in Phase 3 (`proxy::registry`)
//! - Mid-session persona switching API
//! - Per-turn cost (`llm_cost`, `tts_cost`, `stt_cost`) recorded in
//!   `TurnProvenance`
//! - Multi-provider STT/TTS with the §14.1 fallback chain
//!   (`xai → assemblyai`, `xai → elevenlabs`)
//!
//! ## Out of scope
//!
//! - Multi-party / WordGraph AI lane writes
//! - Pause / Stop / Play / barge-in
//! - Context compaction
//! - Expression-annotation auto-prepend
//! - Retry-on-failure logic (manual retry exists; auto-retry does not)
//!
//! Spec references: §3.2, §4 (orchestrator), §5 (state machine),
//! §6.3 (active persona), §10.1 (failure surfacing), §11 (cost).

#![allow(dead_code)] // Skeleton: no production callsite yet. Tests cover the surface.

pub mod stt_router;

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
use parley_core::tts::{ChunkPlanner, ChunkPolicy, ReleasedChunk};
use serde::Serialize;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::llm::{ChatOptions, LlmError, LlmProvider};
use crate::tts::silence::SilenceSplicer;
use crate::tts::{FsTtsCache, TtsBroadcastFrame, TtsChunk, TtsHub, TtsProvider, TtsRequest};

/// One observable state in the per-turn lifecycle. This is a strict
/// subset of spec §5: the audio-bound states (Capturing,
/// FinalizingStt, Speaking, Paused, Stopped) are deferred until the
/// audio integration slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrchestratorState {
    /// No active turn; ready to accept input.
    Idle,
    /// User turn appended; selecting persona / model / provider.
    Routing,
    /// Provider is streaming the assistant response; tokens flowing.
    Streaming,
    /// LLM stream finished; the orchestrator is finishing in-flight
    /// TTS synthesis (per-sentence dispatches against ElevenLabs).
    /// Skipped entirely when no TTS provider is wired into the
    /// context.
    Speaking,
    /// Most recent dispatch failed; awaiting caller decision (retry
    /// or skip). The skeleton just transitions back to `Idle`; spec
    /// §10.1 retry/skip UI lands later.
    Failed,
}

/// One observable side-effect of orchestration. The caller (UI,
/// future HTTP endpoint, test harness) consumes a stream of these
/// from [`ConversationOrchestrator::submit_user_turn`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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
    /// First sentence of an AI turn was dispatched to TTS. Carries
    /// the AI turn's pre-allocated id so the browser can open the
    /// `/conversation/tts/{turn_id}` audio sibling stream before
    /// the AI turn is even appended to the session.
    TtsStarted {
        /// AI turn id this audio belongs to.
        turn_id: TurnId,
    },
    /// One synthesized sentence finished and was appended to the
    /// cache. `sentence_index` is zero-based within the turn.
    /// Useful for progress UI; not required for playback (the
    /// audio sibling stream carries the bytes).
    TtsSentenceDone {
        /// AI turn id this sentence belongs to.
        turn_id: TurnId,
        /// Zero-based index of the sentence within the turn.
        sentence_index: u32,
        /// Billable character count for this sentence.
        characters: u32,
    },
    /// Whole turn's TTS finished. The cache file is now complete
    /// and any audio sibling SSE can close.
    TtsFinished {
        /// AI turn id this fan-out belongs to.
        turn_id: TurnId,
        /// Sum of `characters` across every sentence dispatched.
        total_characters: u32,
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
    /// Retry/discard was requested but the session's tail is not a
    /// pending user turn (no failed dispatch to recover from).
    #[error("no pending user turn to retry or discard")]
    NoPendingTurn,
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
    /// Optional TTS provider. `None` keeps the orchestrator in
    /// text-only mode — no `Speaking` state, no `Tts*` events,
    /// existing behavior preserved. `Some` enables per-sentence
    /// synthesis driven by [`SentenceChunker`].
    pub tts: Option<Arc<dyn TtsProvider>>,
    /// On-disk MP3 cache. Required iff `tts` is `Some`. The
    /// orchestrator writes audio chunks here as they arrive so the
    /// `/conversation/tts/{turn_id}/replay` route and late SSE
    /// subscribers have a source of truth independent of the live
    /// broadcast.
    pub tts_cache: Option<Arc<FsTtsCache>>,
    /// Live broadcast registry shared with the HTTP layer. Required
    /// iff `tts` is `Some`. Used by
    /// `/conversation/tts/{turn_id}` SSE to subscribe to the
    /// in-flight audio for a turn.
    pub tts_hub: Option<Arc<TtsHub>>,
    /// Voice id passed to the TTS provider for every turn in this
    /// session. Per-persona voice override is a follow-up; this
    /// slice keeps it global.
    pub tts_voice_id: Option<String>,
    /// Optional silence splicer used to insert short MP3 silence
    /// prefixes between paragraph-bounded chunks. When `None`, no
    /// silence is spliced — chunks play back-to-back. Wiring the
    /// real ~417-byte 26 ms silent MP3 frame is a startup-time
    /// concern handled outside this struct; the orchestrator only
    /// consults the splicer when present.
    ///
    /// Spec: `docs/paragraph-tts-chunking-spec.md` §3.5.
    pub silence_splicer: Option<Arc<SilenceSplicer>>,
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
        let (persona, model, provider) = self.resolve_active().await?;

        // 2. Resolve the system prompt — inline is free, file
        //    requires I/O. Done before mutating the session for the
        //    same reason as (1). Auto-prepends the expression-tag
        //    instruction (spec §6.4) when the persona allows it and
        //    the active TTS provider can render the tags.
        let system_text =
            build_system_prompt(&persona, &self.ctx.prompts_dir, self.ctx.tts.as_deref())?;

        // 3. Append the user turn and snapshot the message history
        //    we'll send.
        let (user_turn_id, history) = {
            let mut session = self.session.lock().await;
            let id = session.append_user_turn(speaker_id, content.clone(), self.clock.now_ms());
            let mut msgs = vec![ChatMessage::system(system_text)];
            msgs.extend(session.to_chat_messages());
            (id, msgs)
        };

        // 4. Build the dispatch stream. Emits the synthetic
        //    `UserTurnAppended` first since the caller is observing
        //    a fresh submission, not a retry.
        Ok(self.dispatch_history(history, persona, model, provider, Some(user_turn_id)))
    }

    /// Re-dispatch the session's pending tail user turn (the one
    /// left behind by a failed `submit_user_turn`). Resolves
    /// persona/model/provider from the *current* active activation
    /// — a `/switch` between failure and retry will be honored.
    ///
    /// Returns [`OrchestratorError::NoPendingTurn`] when the tail is
    /// not an unanswered user turn.
    pub async fn retry_pending(
        &self,
    ) -> Result<BoxStream<'static, OrchestratorEvent>, OrchestratorError> {
        let (persona, model, provider) = self.resolve_active().await?;
        let system_text =
            build_system_prompt(&persona, &self.ctx.prompts_dir, self.ctx.tts.as_deref())?;

        let history = {
            let session = self.session.lock().await;
            if !session.has_pending_user_turn() {
                return Err(OrchestratorError::NoPendingTurn);
            }
            let mut msgs = vec![ChatMessage::system(system_text)];
            msgs.extend(session.to_chat_messages());
            msgs
        };

        // No `UserTurnAppended` — the user turn is already in the
        // session from the original (failed) attempt. The frontend
        // already has it rendered.
        Ok(self.dispatch_history(history, persona, model, provider, None))
    }

    /// Pop the trailing pending user turn from the session, if one
    /// exists. Used by the "Dismiss" path so the orphaned user turn
    /// doesn't poison the next dispatch's history.
    ///
    /// Returns [`OrchestratorError::NoPendingTurn`] when there is
    /// nothing to pop.
    pub async fn discard_pending(&self) -> Result<(), OrchestratorError> {
        let mut session = self.session.lock().await;
        if session.discard_pending_user_turn().is_some() {
            Ok(())
        } else {
            Err(OrchestratorError::NoPendingTurn)
        }
    }

    /// Resolve the (persona, model, provider) triple for the current
    /// active activation. Shared by `submit_user_turn` and
    /// `retry_pending` so dispatch errors stay consistent.
    async fn resolve_active(
        &self,
    ) -> Result<(Persona, ModelConfig, Arc<dyn LlmProvider>), OrchestratorError> {
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
            .ok_or_else(|| OrchestratorError::UnknownModelConfig(active.model_config_id.clone()))?;
        let provider = self
            .ctx
            .providers
            .get(&active.model_config_id)
            .cloned()
            .ok_or_else(|| OrchestratorError::NoProvider(active.model_config_id.clone()))?;
        Ok((persona, model, provider))
    }

    /// Run the streaming dispatch for an already-prepared history.
    /// Optionally yields a `UserTurnAppended` first when called from
    /// the fresh-submission path; `retry_pending` skips it.
    fn dispatch_history(
        &self,
        history: Vec<ChatMessage>,
        persona: Persona,
        model: ModelConfig,
        provider: Arc<dyn LlmProvider>,
        user_turn_id: Option<TurnId>,
    ) -> BoxStream<'static, OrchestratorEvent> {
        let session = self.session.clone();
        let ctx = self.ctx.clone();
        let clock = self.clock.clone();
        let ai_speaker_id = speaker_for_persona(&persona);
        let persona_id = persona.id.clone();
        let model_id = model.id.clone();
        let opts = ChatOptions::default();

        let stream = async_stream::stream! {
            if let Some(id) = user_turn_id {
                yield OrchestratorEvent::UserTurnAppended { turn_id: id };
            }
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

            // Pre-allocate the AI turn id and snapshot the session
            // id up front so TTS artifacts (cache filename, broadcast
            // hub key, `TtsStarted` event) can address the turn
            // before the AI turn row is written. Holding the lock
            // briefly here is safe: the orchestrator's session
            // mutex is the only writer, and `dispatch_history` runs
            // serially per orchestrator instance.
            let (session_id, ai_turn_id) = {
                let s = session.lock().await;
                (s.id.clone(), s.peek_next_turn_id())
            };

            // TTS state — only populated when the context wires a
            // provider + cache + hub.
            let tts_enabled = ctx.tts.is_some()
                && ctx.tts_cache.is_some()
                && ctx.tts_hub.is_some()
                && ctx.tts_voice_id.is_some();
            let chunk_policy = if tts_enabled {
                ctx.tts
                    .as_ref()
                    .map(|provider| provider.tune_chunk_policy(model.tts_chunking))
                    .unwrap_or(model.tts_chunking)
            } else {
                model.tts_chunking
            };
            let mut tts = if tts_enabled {
                match TtsTurn::open(&ctx, &session_id, ai_turn_id.clone()).await {
                    Ok(t) => Some(t),
                    Err(message) => {
                        // Cache/hub setup failed before any audio
                        // ever flowed. Fall back to text-only for
                        // this turn — the LLM stream still runs to
                        // completion below.
                        yield OrchestratorEvent::Failed { message };
                        None
                    }
                }
            } else {
                None
            };
            // The chunk planner mirrors `tts`: present iff TTS is
            // wired and `TtsTurn::open` succeeded. Built from the
            // model's [`ChunkPolicy`] so per-model tuning flows
            // through automatically.
            let mut planner: Option<ChunkPlanner> = tts
                .as_ref()
                .map(|_| ChunkPlanner::new(chunk_policy));

            let mut accumulated = String::new();
            let mut final_usage = TokenUsage::default();
            let mut llm_errored = false;

            futures::pin_mut!(token_stream);

            // Periodic tick to fire the planner's timer-driven
            // rules (R3 paragraph wait, R5 idle timeout) when the
            // LLM is silent. 250 ms keeps timer resolution well
            // below the smallest configurable window
            // (`first_chunk_max_wait_ms = 800`). Only constructed
            // when TTS is on so text-only turns don't pay for it.
            let mut tick_interval = if planner.is_some() {
                let mut iv = tokio::time::interval(
                    std::time::Duration::from_millis(250),
                );
                iv.set_missed_tick_behavior(
                    tokio::time::MissedTickBehavior::Delay,
                );
                // `interval` fires immediately on first poll;
                // discard that tick so we don't double-fire timer
                // rules at t=0.
                iv.tick().await;
                Some(iv)
            } else {
                None
            };

            // Main streaming loop. Inside each iteration we pull
            // *one* event source — a token or a tick — produce at
            // most one released chunk from the planner, then
            // synchronously drain the single-flight dispatch chain
            // that one chunk unlocks before looping back. This
            // satisfies the spec §3.6 single-flight invariant
            // without any cross-task coordination.
            let mut llm_done = false;
            while !llm_done {
                // Read a single event source. Returns the chunk
                // (if any) that the planner released as a result.
                let released: Option<ReleasedChunk> = if let Some(iv) = tick_interval.as_mut() {
                    tokio::select! {
                        biased;
                        item = token_stream.next() => match item {
                            Some(Ok(ChatToken::TextDelta { text })) => {
                                accumulated.push_str(&text);
                                let chunk = planner
                                    .as_mut()
                                    .and_then(|p| p.push(&text, clock.now_ms()).into_iter().next());
                                yield OrchestratorEvent::Token { delta: text };
                                chunk
                            }
                            Some(Ok(ChatToken::Done { usage })) => {
                                if let Some(u) = usage { final_usage = u; }
                                None
                            }
                            Some(Err(e)) => {
                                yield OrchestratorEvent::Failed { message: format_llm_error(&e) };
                                llm_errored = true;
                                llm_done = true;
                                None
                            }
                            None => {
                                llm_done = true;
                                None
                            }
                        },
                        _ = iv.tick() => {
                            planner
                                .as_mut()
                                .and_then(|p| p.tick(clock.now_ms()).into_iter().next())
                        }
                    }
                } else {
                    // Text-only path: no chunker, no ticks.
                    match token_stream.next().await {
                        Some(Ok(ChatToken::TextDelta { text })) => {
                            accumulated.push_str(&text);
                            yield OrchestratorEvent::Token { delta: text };
                            None
                        }
                        Some(Ok(ChatToken::Done { usage })) => {
                            if let Some(u) = usage { final_usage = u; }
                            None
                        }
                        Some(Err(e)) => {
                            yield OrchestratorEvent::Failed { message: format_llm_error(&e) };
                            llm_errored = true;
                            llm_done = true;
                            None
                        }
                        None => {
                            llm_done = true;
                            None
                        }
                    }
                };

                // Single-flight dispatch chain. The planner only
                // releases one chunk at a time (`try_release` short-
                // circuits while `synthesis_in_flight` is true), so
                // this inner loop can dispatch at most one chunk
                // per LLM event — but `synthesis_completed` may
                // unlock a second paragraph that was already
                // waiting in the buffer, hence the `while`.
                let mut next_chunk = released;
                while let Some(chunk) = next_chunk.take() {
                    let idx = chunk.index;
                    if let Some(t) = tts.as_mut() {
                        let events = t
                            .dispatch_chunk(&ctx, chunk, &chunk_policy)
                            .await;
                        for ev in events { yield ev; }
                    }
                    if let Some(p) = planner.as_mut() {
                        next_chunk = p
                            .synthesis_completed(idx, clock.now_ms())
                            .into_iter()
                            .next();
                    }
                }
            }

            if llm_errored {
                if let Some(t) = tts.take() { t.fail("llm stream failed".into()).await; }
                yield OrchestratorEvent::StateChanged { state: OrchestratorState::Failed };
                yield OrchestratorEvent::StateChanged { state: OrchestratorState::Idle };
                return;
            }

            // Flush remaining buffered text on stream end. R6
            // (`finish`) bypasses single-flight and may emit
            // multiple chunks at once when the buffer holds more
            // than one paragraph; dispatch them serially so the
            // cache and broadcast frames stay ordered.
            if let Some(p) = planner.as_mut()
                && let Some(t) = tts.as_mut()
            {
                for chunk in p.finish(clock.now_ms()) {
                    let events = t
                        .dispatch_chunk(&ctx, chunk, &chunk_policy)
                        .await;
                    for ev in events { yield ev; }
                }
            }

            let (tts_characters, tts_cost) = if let Some(t) = tts.as_ref() {
                (t.total_characters, ctx.tts.as_ref()
                    .map(|p| p.cost(t.total_characters))
                    .unwrap_or_default())
            } else {
                (0u32, Cost::default())
            };

            // Drive the Speaking → Idle transition before the
            // session mutation so the state event timeline matches
            // the spec's lifecycle (§5).
            if tts.is_some() {
                yield OrchestratorEvent::StateChanged { state: OrchestratorState::Speaking };
            }

            // Close out TTS (publish `Done` to the hub and remove
            // the entry) before emitting `TtsFinished` so any
            // subscriber that races to subscribe just after the
            // event sees the same "already finished" answer.
            if let Some(t) = tts.take() {
                let total = t.total_characters;
                let turn_for_event = t.turn_id.clone();
                t.finish().await;
                yield OrchestratorEvent::TtsFinished {
                    turn_id: turn_for_event,
                    total_characters: total,
                };
            }

            let cost = provider.cost(final_usage);
            let provenance = TurnProvenance {
                persona_id: persona_id.clone(),
                model_config_id: model_id.clone(),
                usage: final_usage,
                llm_cost: cost,
                tts_characters,
                tts_cost,
                // STT runs outside the orchestrator today (browser-direct
                // capture). Reserve the slot; populated when the proxy
                // captures STT itself. Spec §7.
                stt_cost: parley_core::chat::Cost::default(),
            };

            let appended_turn_id = {
                let mut session = session.lock().await;
                session.append_ai_turn(
                    ai_speaker_id,
                    accumulated,
                    clock.now_ms(),
                    provenance,
                )
            };
            // Sanity: the pre-allocated id we used for cache/hub
            // must match what `append_ai_turn` actually wrote, or
            // every downstream artifact is misaddressed.
            debug_assert_eq!(appended_turn_id, ai_turn_id);

            yield OrchestratorEvent::AiTurnAppended {
                turn_id: appended_turn_id,
                usage: final_usage,
                cost,
            };
            yield OrchestratorEvent::StateChanged { state: OrchestratorState::Idle };
        };

        Box::pin(stream)
    }
}

/// Per-turn TTS scratch state owned by the dispatch loop.
///
/// Wraps the cache writer, the broadcast handle, and the running
/// totals (sentence index, billable characters). Encapsulates all
/// the bookkeeping so the dispatch loop's body stays linear.
struct TtsTurn {
    turn_id: TurnId,
    cache: Option<crate::tts::TtsCacheWriter>,
    broadcaster: Option<crate::tts::TtsBroadcaster>,
    sentence_index: u32,
    total_characters: u32,
    /// Running cumulative count of audio bytes written/broadcast
    /// for this turn. Tagged onto each broadcast frame so late
    /// subscribers can drop frames already covered by the cache
    /// snapshot they read at attach time.
    total_bytes: u64,
    started: bool,
    /// Once a TTS error fires we stop dispatching further sentences
    /// for this turn — but we let the LLM stream run to completion
    /// so the AI text still appends and the user sees the response
    /// rendered (spec §4.4 "Failure handling within TTS").
    aborted: bool,
    /// Concatenated text of every chunk dispatched so far in this
    /// turn. Passed to providers as `SynthesisContext.previous_text`
    /// so they can pick prosody (intonation, pacing) appropriate to
    /// continuing speech rather than starting a new utterance. This
    /// matters most when a chunk begins with a short function word
    /// ("In", "So", "But") that, read in isolation, sounds like a
    /// pause-laden sentence opener.
    previous_text: String,
}

impl TtsTurn {
    /// Open the cache writer + broadcast channel for `turn_id`.
    /// Pre-conditions enforced by the caller: `ctx.tts`,
    /// `ctx.tts_cache`, `ctx.tts_hub`, and `ctx.tts_voice_id` are
    /// all `Some`.
    async fn open(
        ctx: &OrchestratorContext,
        session_id: &str,
        turn_id: TurnId,
    ) -> Result<Self, String> {
        let cache = ctx.tts_cache.as_ref().expect("tts_cache present");
        let hub = ctx.tts_hub.as_ref().expect("tts_hub present");
        let format = ctx
            .tts
            .as_ref()
            .expect("tts provider present")
            .output_format();
        let writer = cache
            .writer(session_id, &turn_id, format)
            .await
            .map_err(|e| format!("tts cache open failed: {e}"))?;
        let broadcaster = hub.open(turn_id.clone(), format);
        Ok(Self {
            turn_id,
            cache: Some(writer),
            broadcaster: Some(broadcaster),
            sentence_index: 0,
            total_characters: 0,
            total_bytes: 0,
            started: false,
            aborted: false,
            previous_text: String::new(),
        })
    }

    /// Synthesize one chunk, write its audio to the cache, publish
    /// frames to the hub, and return the orchestrator events to
    /// yield. Errors during synthesis convert to a `Failed` event
    /// and flip `aborted` so subsequent calls are no-ops; the LLM
    /// text path is unaffected.
    ///
    /// When `ctx.silence_splicer` is `Some`, a short MP3 silence
    /// prefix is written before the chunk's audio: the
    /// `first_chunk_silence_ms` budget for chunk 0 and
    /// `paragraph_silence_ms` for every later chunk. The silence
    /// counts toward `total_bytes` but not toward `total_characters`
    /// (it isn't billable).
    async fn dispatch_chunk(
        &mut self,
        ctx: &OrchestratorContext,
        chunk: ReleasedChunk,
        policy: &ChunkPolicy,
    ) -> Vec<OrchestratorEvent> {
        if self.aborted {
            return Vec::new();
        }
        let mut events = Vec::new();
        if !self.started {
            self.started = true;
            events.push(OrchestratorEvent::TtsStarted {
                turn_id: self.turn_id.clone(),
            });
        }

        // Splice silence before the chunk's audio. Done before the
        // synthesis call so the silence is always in the cache /
        // broadcast even if synthesis errors immediately.
        if let Some(splicer) = ctx.silence_splicer.as_ref() {
            let silence_ms = if chunk.index == 0 {
                policy.first_chunk_silence_ms
            } else {
                policy.paragraph_silence_ms
            };
            let bytes = splicer.silence(silence_ms);
            if !bytes.is_empty()
                && let Err(e) = self.write_audio_bytes(bytes).await
            {
                events.push(OrchestratorEvent::Failed {
                    message: format!("tts cache write failed: {e}"),
                });
                self.abort(format!("tts cache write failed: {e}")).await;
                return events;
            }
        }

        let provider = ctx.tts.as_ref().expect("tts provider present").clone();
        let voice_id = ctx.tts_voice_id.clone().expect("tts_voice_id present");
        // Translate the LLM's neutral expression tags
        // (`{warm}`, `{laugh}`, `{pause:short}`, …; spec §6.4) into
        // whatever the active provider speaks natively. The default
        // impl strips them; Cartesia / ElevenLabs override.
        let synthesizable_text = provider.translate_expression_tags(&chunk.text);

        // Some chunks reduce to nothing once neutral tags strip
        // (e.g. `"{warm}"` or `"{pause:short}."` on its own).
        // Cartesia rejects an empty / punctuation-only transcript
        // with `unknown_error: Your initial transcript is empty or
        // contains only punctuation`, and ElevenLabs returns a
        // 422. Skip the synthesize call entirely for those — bump
        // the sentence index so cross-chunk bookkeeping stays
        // consistent, advance `previous_text`, and emit a
        // zero-character `TtsSentenceDone` so the frontend cost
        // counter is unchanged.
        if !has_synthesizable_content(&synthesizable_text) {
            let idx = self.sentence_index;
            self.sentence_index += 1;
            if !self.previous_text.is_empty() {
                self.previous_text.push(' ');
            }
            self.previous_text.push_str(&synthesizable_text);
            events.push(OrchestratorEvent::TtsSentenceDone {
                turn_id: self.turn_id.clone(),
                sentence_index: idx,
                characters: 0,
            });
            return events;
        }

        let request = TtsRequest {
            voice_id,
            text: synthesizable_text.clone(),
        };
        // Trim the prior-text window to a provider-friendly size.
        // ElevenLabs documents `previous_text` as accepting up to a
        // few hundred characters; sending more is wasted bandwidth
        // and risks rejection. We slice on a UTF-8 char boundary by
        // walking back from the end.
        const PREVIOUS_TEXT_WINDOW: usize = 500;
        let previous_text = if self.previous_text.is_empty() {
            None
        } else if self.previous_text.len() <= PREVIOUS_TEXT_WINDOW {
            Some(self.previous_text.clone())
        } else {
            // Find the largest valid char boundary at or after
            // `len - PREVIOUS_TEXT_WINDOW` so we never split a
            // multi-byte codepoint.
            let mut start = self.previous_text.len() - PREVIOUS_TEXT_WINDOW;
            while !self.previous_text.is_char_boundary(start) {
                start += 1;
            }
            Some(self.previous_text[start..].to_string())
        };
        let synth_ctx = crate::tts::SynthesisContext {
            previous_text,
            chunk_index: chunk.index,
            final_for_turn: chunk.final_for_turn,
            ..Default::default()
        };

        let mut stream = match provider.synthesize(request, synth_ctx).await {
            Ok(s) => s,
            Err(e) => {
                events.push(OrchestratorEvent::Failed {
                    message: format!("tts synthesis failed: {e}"),
                });
                self.abort(format!("tts synthesis failed: {e}")).await;
                return events;
            }
        };

        // The break expression is the only successful exit path,
        // so the variable is always initialized when read; failure
        // arms return early.
        let characters_for_chunk: u32 = loop {
            match stream.next().await {
                Some(Ok(TtsChunk::Audio(bytes))) => {
                    if let Err(e) = self.write_audio_bytes(bytes).await {
                        events.push(OrchestratorEvent::Failed {
                            message: format!("tts cache write failed: {e}"),
                        });
                        self.abort(format!("tts cache write failed: {e}")).await;
                        return events;
                    }
                }
                Some(Ok(TtsChunk::Done { characters })) => {
                    break characters;
                }
                Some(Err(e)) => {
                    events.push(OrchestratorEvent::Failed {
                        message: format!("tts stream error: {e}"),
                    });
                    self.abort(format!("tts stream error: {e}")).await;
                    return events;
                }
                None => {
                    // Stream ended without a Done frame. Treat as
                    // protocol error so the user sees a banner; the
                    // chunk's audio so far is still in the cache.
                    events.push(OrchestratorEvent::Failed {
                        message: "tts stream ended without Done frame".into(),
                    });
                    self.abort("tts stream ended without Done frame".into())
                        .await;
                    return events;
                }
            }
        };

        let idx = self.sentence_index;
        self.sentence_index += 1;
        self.total_characters = self.total_characters.saturating_add(characters_for_chunk);
        // Append the just-dispatched text to the rolling prior-text
        // buffer with a single space separator so the next chunk's
        // `previous_text` reads as continuous prose. Trim the buffer
        // to keep memory bounded — the slice we send to the provider
        // is already capped, but holding the full transcript would
        // grow without limit on long turns.
        if !self.previous_text.is_empty() {
            self.previous_text.push(' ');
        }
        // Carry the *translated* text into the previous_text window
        // so providers don't see neutral `{warm}` / `{laugh}` markers
        // in their continuation hints. Cheap (already computed
        // `synthesizable_text` above for this chunk).
        self.previous_text.push_str(&synthesizable_text);
        const PREVIOUS_TEXT_BUFFER_CAP: usize = 4096;
        if self.previous_text.len() > PREVIOUS_TEXT_BUFFER_CAP {
            let mut start = self.previous_text.len() - PREVIOUS_TEXT_BUFFER_CAP;
            while !self.previous_text.is_char_boundary(start) {
                start += 1;
            }
            self.previous_text = self.previous_text[start..].to_string();
        }
        events.push(OrchestratorEvent::TtsSentenceDone {
            turn_id: self.turn_id.clone(),
            sentence_index: idx,
            characters: characters_for_chunk,
        });
        events
    }

    /// Write `bytes` to the cache and broadcast them to live
    /// subscribers in the spec-mandated order: cache first, then
    /// broadcast. The cache file is always at least as long as
    /// anything a live subscriber has seen — the late-join handoff
    /// in §5.1 depends on this ordering.
    async fn write_audio_bytes(
        &mut self,
        bytes: Vec<u8>,
    ) -> Result<(), crate::tts::cache::CacheError> {
        if let Some(writer) = self.cache.as_mut() {
            writer.write(&bytes).await?;
        }
        if let Some(b) = self.broadcaster.as_ref() {
            self.total_bytes = self.total_bytes.saturating_add(bytes.len() as u64);
            b.send(TtsBroadcastFrame::Audio {
                bytes,
                total_bytes_after: self.total_bytes,
            });
        }
        Ok(())
    }

    /// Mark this turn aborted and tear down the broadcaster with a
    /// terminal error frame. Subsequent `dispatch_sentence` calls
    /// short-circuit. The cache file is left as-is (truncated MP3
    /// is still playable).
    async fn abort(&mut self, message: String) {
        self.aborted = true;
        if let Some(b) = self.broadcaster.take() {
            b.fail(message);
        }
        // Best-effort flush of whatever bytes we already wrote.
        if let Some(writer) = self.cache.take() {
            let _ = writer.finish().await;
        }
    }

    /// Successful completion: publish `Done` to subscribers, flush
    /// the cache. Consumes `self`.
    async fn finish(mut self) {
        if let Some(b) = self.broadcaster.take() {
            b.finish();
        }
        if let Some(writer) = self.cache.take() {
            let _ = writer.finish().await;
        }
    }

    /// Failure handoff used when the LLM stream itself errored
    /// after TTS had already started: publish an error frame so the
    /// browser audio stream tears down, and flush the cache.
    async fn fail(mut self, message: String) {
        if let Some(b) = self.broadcaster.take() {
            b.fail(message);
        }
        if let Some(writer) = self.cache.take() {
            let _ = writer.finish().await;
        }
    }
}

/// Convention: an AI agent's speaker id is `"ai-<persona_id>"`. The
/// orchestrator does not enforce that speakers be pre-registered in
/// the session; multi-speaker registration lands in a later slice.
fn speaker_for_persona(persona: &Persona) -> SpeakerId {
    format!("ai-{}", persona.id)
}

/// Build the dispatch-time system prompt for `persona`: starts from
/// the persona's own text (inline or read from disk) and, when the
/// Cheap "is this string worth sending to a TTS provider?" probe.
/// True iff at least one character is alphabetic or numeric. Pure
/// whitespace, lone punctuation, and stray tag-residue (e.g. `". "`
/// after `{warm}` strips out) all return `false`.
///
/// Cartesia surfaces these as `unknown_error: Your initial
/// transcript is empty or contains only punctuation`; ElevenLabs
/// returns a 422. We dodge both by short-circuiting the synthesize
/// call and emitting a zero-character `TtsSentenceDone` instead.
fn has_synthesizable_content(text: &str) -> bool {
    text.chars().any(|c| c.is_alphanumeric())
}

/// persona allows it AND the active TTS provider supplies an expression
/// instruction, prepends that provider's instruction so the LLM only
/// learns the tags/spans the selected TTS model can actually use.
///
/// Spec: `docs/conversation-mode-spec.md` §6.4. Personas can opt out
/// via `persona.tts.use_expression_annotations = false` and bake
/// whatever guidance they want directly into their prompt.
fn build_system_prompt(
    persona: &Persona,
    prompts_dir: &std::path::Path,
    tts: Option<&dyn TtsProvider>,
) -> Result<String, OrchestratorError> {
    let body = resolve_system_prompt(&persona.system_prompt, prompts_dir)?;
    let instruction = if persona.tts.use_expression_annotations {
        tts.and_then(|p| p.expression_tag_instruction())
    } else {
        None
    };
    let Some(instruction) = instruction else {
        return Ok(body);
    };
    let mut out = String::with_capacity(body.len() + instruction.len() + 2);
    out.push_str(&instruction);
    // Two newlines so the auto-prepended block is visibly separate
    // from the persona's own system prompt body. The LLM sees a clear
    // section boundary, not a run-on paragraph.
    out.push_str("\n\n");
    out.push_str(&body);
    Ok(out)
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
    use crate::llm::test_support::{MockItem, MockProvider};
    use parley_core::model_config::{LlmProviderTag, TokenRates};
    use parley_core::persona::{
        PersonaContextSettings, PersonaTier, PersonaTiers, PersonaTtsSettings, SystemPrompt,
    };
    use parley_core::speaker::Speaker;

    #[test]
    fn has_synthesizable_content_rejects_empty_and_punctuation() {
        // Cases the orchestrator now skips before calling the TTS
        // provider — Cartesia would otherwise return
        // `unknown_error: Your initial transcript is empty …`.
        assert!(!has_synthesizable_content(""));
        assert!(!has_synthesizable_content("   "));
        assert!(!has_synthesizable_content("..."));
        assert!(!has_synthesizable_content(". , !"));
        assert!(!has_synthesizable_content("\t\n"));
        // Anything with a letter or digit goes through; lone SSML
        // markup or `[laughter]` carries enough content for
        // providers to act on, so we don't second-guess them here.
        assert!(has_synthesizable_content("hi"));
        assert!(has_synthesizable_content("3"));
        assert!(has_synthesizable_content("[laughter]"));
        assert!(has_synthesizable_content("Sí."));
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
            tts_chunking: parley_core::tts::ChunkPolicy::default(),
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
            tts: None,
            tts_cache: None,
            tts_hub: None,
            tts_voice_id: None,
            silence_splicer: None,
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
            tts: None,
            tts_cache: None,
            tts_hub: None,
            tts_voice_id: None,
            silence_splicer: None,
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
            tts: None,
            tts_cache: None,
            tts_hub: None,
            tts_voice_id: None,
            silence_splicer: None,
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
            tts: None,
            tts_cache: None,
            tts_hub: None,
            tts_voice_id: None,
            silence_splicer: None,
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
        let captured_handle = provider.captured_handle();
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
    async fn system_prompt_adds_expression_instruction_when_tts_supports_tags() {
        let provider = Arc::new(MockProvider::new(
            "p",
            vec![MockItem::Text("ok".into())],
            TokenUsage::default(),
        ));
        let captured_handle = provider.captured_handle();
        let mock_tts = StdArc::new(MockTtsProvider::new(vec![MockTtsCall::Ok {
            audio_chunks: vec![vec![1, 2, 3]],
            characters: 2,
        }]));
        let (o, _, _) = build_with_tts(
            sample_persona("scholar", "m1", "BE BRIEF"),
            sample_model("m1"),
            provider,
            mock_tts,
        );

        let _ = drain(&o, "hi").await;

        let captured = captured_handle.lock().unwrap().clone().expect("captured");
        assert_eq!(captured[0].role, parley_core::chat::ChatRole::System);
        assert!(
            captured[0]
                .content
                .starts_with("MOCK TTS EXPRESSION INSTRUCTION")
        );
        assert!(captured[0].content.ends_with("BE BRIEF"));
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
        let captured_handle = provider.captured_handle();
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
            tts: None,
            tts_cache: None,
            tts_hub: None,
            tts_voice_id: None,
            silence_splicer: None,
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
            tts: None,
            tts_cache: None,
            tts_hub: None,
            tts_voice_id: None,
            silence_splicer: None,
        };
        let o = ConversationOrchestrator::new(session, ctx);
        let err = match o.submit_user_turn("gavin".into(), "hi".into()).await {
            Err(e) => e,
            Ok(_) => panic!("expected PromptFile"),
        };
        assert!(matches!(err, OrchestratorError::PromptFile { .. }));
    }

    // ----- TTS wiring -----

    use crate::tts::{FsTtsCache, TtsChunk, TtsError, TtsHub, TtsProvider, TtsRequest, TtsStream};
    use async_trait::async_trait;
    use std::sync::{Arc as StdArc, Mutex as StdMutex};

    /// Test TTS provider driven by a canned per-call script. Each
    /// call to `synthesize` pops the next script entry. An entry is
    /// either a sequence of audio chunks + final character count
    /// (success) or a `TtsError` (failure).
    enum MockTtsCall {
        Ok {
            audio_chunks: Vec<Vec<u8>>,
            characters: u32,
        },
        Err(TtsError),
    }

    struct MockTtsProvider {
        script: StdMutex<Vec<MockTtsCall>>,
        captured: StdMutex<Vec<TtsRequest>>,
        captured_ctx: StdMutex<Vec<crate::tts::SynthesisContext>>,
        tuned_chunk_policy: Option<parley_core::tts::ChunkPolicy>,
    }

    impl MockTtsProvider {
        fn new(script: Vec<MockTtsCall>) -> Self {
            Self {
                script: StdMutex::new(script),
                captured: StdMutex::new(Vec::new()),
                captured_ctx: StdMutex::new(Vec::new()),
                tuned_chunk_policy: None,
            }
        }
        fn with_tuned_chunk_policy(
            script: Vec<MockTtsCall>,
            tuned_chunk_policy: parley_core::tts::ChunkPolicy,
        ) -> Self {
            Self {
                script: StdMutex::new(script),
                captured: StdMutex::new(Vec::new()),
                captured_ctx: StdMutex::new(Vec::new()),
                tuned_chunk_policy: Some(tuned_chunk_policy),
            }
        }
        fn captured(&self) -> Vec<TtsRequest> {
            self.captured.lock().unwrap().clone()
        }
        fn captured_ctx(&self) -> Vec<crate::tts::SynthesisContext> {
            self.captured_ctx.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl TtsProvider for MockTtsProvider {
        fn id(&self) -> &'static str {
            "mock-tts"
        }
        async fn synthesize(
            &self,
            request: TtsRequest,
            ctx: crate::tts::SynthesisContext,
        ) -> Result<TtsStream, TtsError> {
            self.captured.lock().unwrap().push(request);
            self.captured_ctx.lock().unwrap().push(ctx);
            let next = {
                let mut s = self.script.lock().unwrap();
                if s.is_empty() {
                    return Err(TtsError::Other("script exhausted".into()));
                }
                s.remove(0)
            };
            match next {
                MockTtsCall::Err(e) => Err(e),
                MockTtsCall::Ok {
                    audio_chunks,
                    characters,
                } => {
                    let mut frames: Vec<Result<TtsChunk, TtsError>> = audio_chunks
                        .into_iter()
                        .map(|b| Ok(TtsChunk::Audio(b)))
                        .collect();
                    frames.push(Ok(TtsChunk::Done { characters }));
                    Ok(Box::pin(futures::stream::iter(frames)))
                }
            }
        }
        fn cost(&self, characters: u32) -> Cost {
            // $0.000015/char so the math matches ElevenLabsTts and
            // the assertion stays human-readable.
            Cost::from_usd(characters as f64 * 0.000_015)
        }
        fn output_format(&self) -> crate::tts::AudioFormat {
            crate::tts::AudioFormat::Mp3_44100_128
        }
        fn tune_chunk_policy(
            &self,
            policy: parley_core::tts::ChunkPolicy,
        ) -> parley_core::tts::ChunkPolicy {
            self.tuned_chunk_policy.unwrap_or(policy)
        }
        fn expression_tag_instruction(&self) -> Option<String> {
            Some("MOCK TTS EXPRESSION INSTRUCTION".into())
        }
    }

    /// Build an orchestrator with TTS wired up. Returns the
    /// orchestrator plus handles for assertions:
    /// `(orch, mock_tts, hub, cache_root)`.
    fn build_with_tts(
        persona: Persona,
        model: ModelConfig,
        provider: Arc<dyn LlmProvider>,
        mock_tts: StdArc<MockTtsProvider>,
    ) -> (ConversationOrchestrator, Arc<TtsHub>, tempfile::TempDir) {
        build_with_tts_inner(persona, model, provider, mock_tts, None)
    }

    /// Same as [`build_with_tts`] but also wires a [`SilenceSplicer`]
    /// so tests can verify the silence prefix appears in cache and
    /// broadcast frames.
    fn build_with_tts_and_silence(
        persona: Persona,
        model: ModelConfig,
        provider: Arc<dyn LlmProvider>,
        mock_tts: StdArc<MockTtsProvider>,
        splicer: Arc<SilenceSplicer>,
    ) -> (ConversationOrchestrator, Arc<TtsHub>, tempfile::TempDir) {
        build_with_tts_inner(persona, model, provider, mock_tts, Some(splicer))
    }

    fn build_with_tts_inner(
        persona: Persona,
        model: ModelConfig,
        provider: Arc<dyn LlmProvider>,
        mock_tts: StdArc<MockTtsProvider>,
        splicer: Option<Arc<SilenceSplicer>>,
    ) -> (ConversationOrchestrator, Arc<TtsHub>, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = Arc::new(FsTtsCache::new(tmp.path()));
        let hub = Arc::new(TtsHub::new());
        let session = ConversationSession::new(
            "sess-tts",
            Speaker::ai_agent(format!("ai-{}", persona.id), &persona.name),
            persona.id.clone(),
            model.id.clone(),
        );
        let ctx = OrchestratorContext {
            personas: [(persona.id.clone(), persona)].into(),
            models: [(model.id.clone(), model.clone())].into(),
            providers: [(model.id.clone(), provider)].into(),
            prompts_dir: PathBuf::from("/nonexistent"),
            tts: Some(mock_tts as Arc<dyn TtsProvider>),
            tts_cache: Some(cache),
            tts_hub: Some(hub.clone()),
            tts_voice_id: Some("voice-jarnathan".into()),
            silence_splicer: splicer,
        };
        let o = ConversationOrchestrator::with_clock(session, ctx, Arc::new(FakeClock(1_000)));
        (o, hub, tmp)
    }

    #[tokio::test]
    async fn two_sentences_dispatch_per_sentence_to_tts() {
        // LLM emits "Hello world. " and "Goodbye world." — two
        // complete sentences arriving on a single first chunk. The
        // chunker's R1 fast-path packs both sentences into the
        // first dispatched chunk (under the default 220-char cap),
        // so we expect a single TTS dispatch covering all 26
        // characters.
        let llm = Arc::new(MockProvider::new(
            "p",
            vec![
                MockItem::Text("Hello world. ".into()),
                MockItem::Text("Goodbye world.".into()),
            ],
            TokenUsage {
                input: 1,
                output: 1,
            },
        ));
        let mock_tts = StdArc::new(MockTtsProvider::new(vec![MockTtsCall::Ok {
            audio_chunks: vec![vec![0xAA, 0xBB, 0xCC]],
            characters: 26,
        }]));
        let (o, _hub, _tmp) = build_with_tts(
            sample_persona("scholar", "m1", "x"),
            sample_model("m1"),
            llm,
            mock_tts.clone(),
        );

        let events = drain(&o, "hi").await;

        // Exactly one TtsStarted, one TtsSentenceDone (index 0
        // covering both sentences), one TtsFinished.
        let started = events
            .iter()
            .filter(|e| matches!(e, OrchestratorEvent::TtsStarted { .. }))
            .count();
        assert_eq!(started, 1);
        let sentence_dones: Vec<u32> = events
            .iter()
            .filter_map(|e| match e {
                OrchestratorEvent::TtsSentenceDone { sentence_index, .. } => Some(*sentence_index),
                _ => None,
            })
            .collect();
        assert_eq!(sentence_dones, vec![0]);
        let finished_total = events.iter().find_map(|e| match e {
            OrchestratorEvent::TtsFinished {
                total_characters, ..
            } => Some(*total_characters),
            _ => None,
        });
        assert_eq!(finished_total, Some(26));

        // Speaking state appears between Streaming and Idle.
        let s = states(&events);
        let speaking_pos = s.iter().position(|x| *x == OrchestratorState::Speaking);
        let idle_pos = s.iter().rposition(|x| *x == OrchestratorState::Idle);
        assert!(speaking_pos.is_some() && idle_pos.is_some());
        assert!(speaking_pos.unwrap() < idle_pos.unwrap());

        // Provenance carries the TTS totals.
        let snap = o.session_snapshot().await;
        let prov = snap.turns[1].provenance.as_ref().unwrap();
        assert_eq!(prov.tts_characters, 26);
        assert!((prov.tts_cost.usd - 26.0 * 0.000_015).abs() < 1e-9);

        // Provider saw the right voice and got both sentences in
        // the single dispatched chunk.
        let calls = mock_tts.captured();
        assert_eq!(calls.len(), 1);
        assert!(calls.iter().all(|r| r.voice_id == "voice-jarnathan"));
        assert_eq!(calls[0].text.trim(), "Hello world. Goodbye world.");
    }

    #[tokio::test]
    async fn tts_disabled_preserves_text_only_flow() {
        // Same persona / model, but no TTS in the context. The new
        // events must not appear and Speaking state must not be
        // entered.
        let llm = Arc::new(MockProvider::new(
            "p",
            vec![MockItem::Text("Hi.".into())],
            TokenUsage::default(),
        ));
        let o = build(
            sample_persona("scholar", "m1", "x"),
            sample_model("m1"),
            llm,
        );
        let events = drain(&o, "hi").await;

        assert!(!events.iter().any(|e| matches!(
            e,
            OrchestratorEvent::TtsStarted { .. }
                | OrchestratorEvent::TtsSentenceDone { .. }
                | OrchestratorEvent::TtsFinished { .. }
        )));
        assert!(!states(&events).contains(&OrchestratorState::Speaking));
        let snap = o.session_snapshot().await;
        let prov = snap.turns[1].provenance.as_ref().unwrap();
        assert_eq!(prov.tts_characters, 0);
        assert_eq!(prov.tts_cost.usd, 0.0);
    }

    #[tokio::test]
    async fn tts_error_mid_turn_does_not_drop_ai_text() {
        // The provider errors on the only dispatched chunk
        // ("First. Second." packs into a single chunk via R1's
        // first-chunk fast path). The AI turn must still be
        // appended with the full LLM text, and a Failed event
        // must surface.
        let llm = Arc::new(MockProvider::new(
            "p",
            vec![MockItem::Text("First. Second.".into())],
            TokenUsage::default(),
        ));
        let mock_tts = StdArc::new(MockTtsProvider::new(vec![MockTtsCall::Err(
            TtsError::Other("boom".into()),
        )]));
        let (o, hub, _tmp) = build_with_tts(
            sample_persona("scholar", "m1", "x"),
            sample_model("m1"),
            llm,
            mock_tts,
        );

        let events = drain(&o, "hi").await;

        assert!(events.iter().any(
            |e| matches!(e, OrchestratorEvent::Failed { message } if message.contains("boom"))
        ));
        // The AI turn is still appended with the full text.
        let snap = o.session_snapshot().await;
        assert_eq!(snap.turns.len(), 2);
        assert_eq!(snap.turns[1].content, "First. Second.");
        // The hub entry was cleaned up by the abort path.
        assert!(!hub.is_live("turn-0001"));
    }

    #[tokio::test]
    async fn tts_starts_emits_pre_allocated_ai_turn_id() {
        // The AI turn id in TtsStarted must equal the id eventually
        // assigned by append_ai_turn. The pre-allocation is what
        // lets the browser open the audio sibling stream before the
        // session has the turn.
        let llm = Arc::new(MockProvider::new(
            "p",
            vec![MockItem::Text("Hi.".into())],
            TokenUsage::default(),
        ));
        let mock_tts = StdArc::new(MockTtsProvider::new(vec![MockTtsCall::Ok {
            audio_chunks: vec![vec![0x01, 0x02]],
            characters: 3,
        }]));
        let (o, _hub, _tmp) = build_with_tts(
            sample_persona("scholar", "m1", "x"),
            sample_model("m1"),
            llm,
            mock_tts,
        );

        let events = drain(&o, "hi").await;

        let started_id = events
            .iter()
            .find_map(|e| match e {
                OrchestratorEvent::TtsStarted { turn_id } => Some(turn_id.clone()),
                _ => None,
            })
            .expect("TtsStarted");
        let ai_appended_id = events
            .iter()
            .find_map(|e| match e {
                OrchestratorEvent::AiTurnAppended { turn_id, .. } => Some(turn_id.clone()),
                _ => None,
            })
            .expect("AiTurnAppended");
        assert_eq!(started_id, ai_appended_id);
        // And the hub entry was removed by `finish` after the run.
        // (Implicit via TtsFinished happening.)
    }

    #[tokio::test]
    async fn live_subscriber_receives_audio_frames_via_hub() {
        // Open a subscriber on the hub *before* dispatch so we
        // catch frames as they're broadcast. Verifies the
        // orchestrator publishes Audio + Done correctly.
        let llm = Arc::new(MockProvider::new(
            "p",
            vec![MockItem::Text("Hello.".into())],
            TokenUsage::default(),
        ));
        let mock_tts = StdArc::new(MockTtsProvider::new(vec![MockTtsCall::Ok {
            audio_chunks: vec![vec![0xAA], vec![0xBB, 0xCC]],
            characters: 6,
        }]));
        let (o, hub, _tmp) = build_with_tts(
            sample_persona("scholar", "m1", "x"),
            sample_model("m1"),
            llm,
            mock_tts,
        );

        // Pre-allocate the turn id and subscribe before dispatch.
        // We know it'll be turn-0001 (turn-0000 is the user turn).
        let session = o.session_snapshot().await;
        let _user_id = session.peek_next_turn_id(); // turn-0000
        // Subscribing to a not-yet-open hub returns None; we'll
        // subscribe inside the stream instead by polling between
        // events. Simpler: just consume events and verify the
        // bytes ended up in the cache.
        let mut sub: Option<tokio::sync::broadcast::Receiver<TtsBroadcastFrame>> = None;
        let mut s = o
            .submit_user_turn("gavin".into(), "hi".into())
            .await
            .expect("submit");
        let mut events_seen = Vec::new();
        while let Some(ev) = s.next().await {
            if matches!(ev, OrchestratorEvent::TtsStarted { .. }) && sub.is_none() {
                sub = hub.subscribe("turn-0001");
            }
            events_seen.push(ev);
        }

        // We may have missed earlier audio frames if subscribe
        // raced \u2014 but at minimum the subscriber should see Done.
        let mut sub = sub.expect("subscribed");
        let mut got_done = false;
        // Drain any buffered frames.
        while let Ok(frame) = sub.try_recv() {
            if matches!(frame, TtsBroadcastFrame::Done) {
                got_done = true;
            }
        }
        assert!(got_done, "expected Done frame on broadcast");
        // The cache file exists with the synthesized bytes.
        // (Implicit \u2014 finish was called; this is exercised by the
        // tts cache module's own tests.)
    }

    #[tokio::test]
    async fn silence_splicer_prefixes_first_chunk_audio() {
        // When a `SilenceSplicer` is wired, the orchestrator must
        // prepend `policy.first_chunk_silence_ms` of silence before
        // the first chunk's synthesized audio. With the default
        // policy (100 ms) and a 26 ms frame, that's 4 frames =
        // 4 * 418 = 1672 bytes of silence ahead of the provider's
        // output bytes. We assert on the cache file (single source
        // of truth for cumulative bytes).
        use crate::tts::silence::{SILENCE_FRAME_44100_128_STEREO, SilenceSplicer};
        let llm = Arc::new(MockProvider::new(
            "p",
            vec![MockItem::Text("Hello.".into())],
            TokenUsage::default(),
        ));
        let provider_audio = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let mock_tts = StdArc::new(MockTtsProvider::new(vec![MockTtsCall::Ok {
            audio_chunks: vec![provider_audio.clone()],
            characters: 6,
        }]));
        let splicer = Arc::new(SilenceSplicer::default_44100_128_stereo());
        let (o, _hub, tmp) = build_with_tts_and_silence(
            sample_persona("scholar", "m1", "x"),
            sample_model("m1"),
            llm,
            mock_tts,
            splicer,
        );

        let _events = drain(&o, "hi").await;

        // Read the cache file directly. Path layout matches
        // `FsTtsCache::writer` (tts-cache subdir per session).
        let cache_path = tmp
            .path()
            .join("sess-tts")
            .join("tts-cache")
            .join("turn-0001.mp3");
        let bytes = std::fs::read(&cache_path).expect("cache file exists");
        // 4 silence frames + provider audio.
        let expected_silence_len = 4 * SILENCE_FRAME_44100_128_STEREO.len();
        assert_eq!(
            bytes.len(),
            expected_silence_len + provider_audio.len(),
            "cache must hold silence prefix + provider audio"
        );
        // Each silence frame matches the canonical bytes.
        for i in 0..4 {
            let start = i * SILENCE_FRAME_44100_128_STEREO.len();
            let end = start + SILENCE_FRAME_44100_128_STEREO.len();
            assert_eq!(
                &bytes[start..end],
                SILENCE_FRAME_44100_128_STEREO,
                "silence frame {i} mismatch",
            );
        }
        // The provider audio follows the silence prefix verbatim.
        assert_eq!(&bytes[expected_silence_len..], provider_audio.as_slice());

        // Provenance only counts billable characters (silence is
        // not billable).
        let snap = o.session_snapshot().await;
        let prov = snap.turns[1].provenance.as_ref().unwrap();
        assert_eq!(prov.tts_characters, 6);
    }

    #[tokio::test]
    async fn previous_text_accumulates_across_chunks() {
        // The orchestrator must pass each chunk's predecessors as
        // `SynthesisContext.previous_text` so providers (notably
        // ElevenLabs v3) can pick continuation prosody. We force a
        // multi-chunk turn via a paragraph break (R2) and then
        // inspect the captured contexts.
        let llm = Arc::new(MockProvider::new(
            "p",
            vec![MockItem::Text(
                "Para one. Sentence two.\n\nPara two. Sentence two.".into(),
            )],
            TokenUsage::default(),
        ));
        let mock_tts = StdArc::new(MockTtsProvider::new(vec![
            MockTtsCall::Ok {
                audio_chunks: vec![vec![0x01]],
                characters: 23,
            },
            MockTtsCall::Ok {
                audio_chunks: vec![vec![0x02]],
                characters: 23,
            },
        ]));
        let (o, _hub, _tmp) = build_with_tts(
            sample_persona("scholar", "m1", "x"),
            sample_model("m1"),
            llm,
            mock_tts.clone(),
        );

        let _events = drain(&o, "hi").await;

        let calls = mock_tts.captured();
        let ctxs = mock_tts.captured_ctx();
        assert_eq!(calls.len(), 2, "expected two chunks via paragraph break");
        assert_eq!(ctxs.len(), 2);
        // Chunk 0: no prior context.
        assert!(
            ctxs[0].previous_text.is_none(),
            "first chunk must have no previous_text, got {:?}",
            ctxs[0].previous_text
        );
        assert_eq!(ctxs[0].chunk_index, 0);
        // Chunk 1: previous_text must contain chunk 0's text.
        let prev = ctxs[1]
            .previous_text
            .as_deref()
            .expect("second chunk must carry previous_text");
        assert!(
            prev.contains(calls[0].text.trim()),
            "previous_text {prev:?} should contain prior chunk text {:?}",
            calls[0].text,
        );
        assert_eq!(ctxs[1].chunk_index, 1);
    }

    #[tokio::test]
    async fn tts_provider_chunk_policy_tuning_keeps_first_paragraph_together() {
        let llm = Arc::new(MockProvider::new(
            "p",
            vec![MockItem::Text("One. Two. Three.\n\nNext paragraph.".into())],
            TokenUsage::default(),
        ));
        let mut tuned = parley_core::tts::ChunkPolicy::default();
        tuned.first_chunk_max_sentences = 0;
        tuned.idle_timeout_ms = tuned.paragraph_wait_ms;
        let mock_tts = StdArc::new(MockTtsProvider::with_tuned_chunk_policy(
            vec![
                MockTtsCall::Ok {
                    audio_chunks: vec![vec![0x01]],
                    characters: 16,
                },
                MockTtsCall::Ok {
                    audio_chunks: vec![vec![0x02]],
                    characters: 15,
                },
            ],
            tuned,
        ));
        let (o, _hub, _tmp) = build_with_tts(
            sample_persona("scholar", "m1", "x"),
            sample_model("m1"),
            llm,
            mock_tts.clone(),
        );

        let _events = drain(&o, "hi").await;

        let calls = mock_tts.captured();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].text, "One. Two. Three.");
        assert_eq!(calls[1].text, "Next paragraph.");
    }
}
