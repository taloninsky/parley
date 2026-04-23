//! HTTP surface for the conversation orchestrator.
//!
//! Exposes a single in-process `ConversationOrchestrator` over HTTP
//! so the WASM frontend (and any other client) can drive a session
//! without speaking to LLM providers directly.
//!
//! ## Endpoints
//!
//! - `POST /conversation/init`     — create the session (one per process for now)
//! - `POST /conversation/turn`     — submit a user turn; SSE-streams `OrchestratorEvent`s
//! - `POST /conversation/switch`   — switch the active (persona, model) for the next turn
//! - `GET  /conversation/snapshot` — full session state as JSON
//! - `GET  /conversation/tts/{turn_id}`         — SSE stream of MP3 audio frames for a turn
//! - `GET  /conversation/tts/{turn_id}/replay`  — raw cached MP3 file (`audio/mpeg`)
//!
//! ## Scope of this slice
//!
//! - Single in-process session. Re-issuing `init` replaces the
//!   previous session. The on-disk session file format is a separate
//!   slice.
//! - Provider construction supports Anthropic only; other
//!   `LlmProviderTag` values return `501 Not Implemented`. The
//!   provider's API key is resolved at request time from the proxy's
//!   [`SecretsManager`] using the optional `credential` field on the
//!   request body (defaults to the `default` credential). Keys are
//!   never logged or echoed.
//! - SSE frames carry one [`OrchestratorEvent`] per `data:` line as
//!   JSON. Stream terminates after `ai_turn_appended` (or after
//!   `failed` followed by the final `state_changed` -> `idle`).
//!
//! Spec references: §4 (orchestrator boundary), §5 (state machine),
//! §10.1 (failure surfacing), §12 (provider abstraction).

#![allow(dead_code)] // Some helpers are exercised only by tests until the frontend wires up.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{delete, get, post},
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use futures::{Stream, StreamExt};
use parley_core::conversation::ConversationSession;
use parley_core::model_config::{LlmProviderTag, ModelConfig, ModelConfigId};
use parley_core::persona::{Persona, PersonaId};
use parley_core::speaker::{Speaker, SpeakerId};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::llm::LlmProvider;
use crate::llm::anthropic::AnthropicLlm;
use crate::orchestrator::{
    ConversationOrchestrator, OrchestratorContext, OrchestratorError, OrchestratorEvent,
};
use crate::providers::ProviderId;
use crate::secrets::{DEFAULT_CREDENTIAL, SecretsManager};
use crate::session_store::{FsSessionStore, SessionStore, SessionStoreError};
use crate::tts::{ElevenLabsTts, FsTtsCache, TtsBroadcastFrame, TtsHub};

/// Default ElevenLabs voice id ("Jarnathan") used when an init or
/// load request omits the `voice_id` field. Spec §6.
pub const DEFAULT_TTS_VOICE_ID: &str = "c6SfcYrb2t09NHXiT80T";

/// Immutable view of the registries loaded once at proxy boot.
/// Cheap to share via `Arc`.
pub struct Registries {
    /// All loaded personas, keyed by id.
    pub personas: HashMap<PersonaId, Persona>,
    /// All loaded model configs, keyed by id.
    pub models: HashMap<ModelConfigId, ModelConfig>,
    /// Directory holding `SystemPrompt::File` referents.
    pub prompts_dir: PathBuf,
    /// Directory the filesystem session store writes into.
    pub sessions_dir: PathBuf,
}

/// Resolved TTS infrastructure shared across requests. The hub is
/// always live (empty until a turn opens it); the cache is rooted
/// at a fixed directory and scopes by session id internally.
#[derive(Clone)]
pub struct TtsRuntime {
    /// Per-turn fan-out registry. Shared with the orchestrator so
    /// dispatch publishes into the same channels the SSE route
    /// subscribes to.
    pub hub: Arc<TtsHub>,
    /// On-disk MP3 cache. Shared with the orchestrator (writer) and
    /// the SSE / replay routes (reader).
    pub cache: Arc<FsTtsCache>,
}

/// Shared state behind the conversation routes.
///
/// `inner` holds the live orchestrator (None until first `init`).
/// `registries` is the immutable view of personas/models loaded at
/// boot. `http` is the shared reqwest client used to construct
/// real providers. `store` persists sessions to disk. `secrets`
/// resolves provider API keys at request time from the proxy's
/// keystore-backed credential vault.
#[derive(Clone)]
pub struct ConversationApiState {
    inner: Arc<Mutex<Option<Arc<ConversationOrchestrator>>>>,
    registries: Arc<Registries>,
    http: reqwest::Client,
    store: Arc<dyn SessionStore>,
    secrets: Arc<SecretsManager>,
    tts: TtsRuntime,
}

impl ConversationApiState {
    /// Build a new state wrapper around the supplied registries,
    /// HTTP client, and secrets manager. Uses a [`FsSessionStore`]
    /// rooted at `registries.sessions_dir` and a [`FsTtsCache`]
    /// rooted at the same directory (per spec §4.3 the cache lives
    /// alongside saved sessions: `{sessions_dir}/{session_id}/tts-cache/`).
    pub fn new(
        registries: Arc<Registries>,
        http: reqwest::Client,
        secrets: Arc<SecretsManager>,
    ) -> Self {
        let store: Arc<dyn SessionStore> =
            Arc::new(FsSessionStore::new(registries.sessions_dir.clone()));
        let tts = TtsRuntime {
            hub: Arc::new(TtsHub::new()),
            cache: Arc::new(FsTtsCache::new(registries.sessions_dir.clone())),
        };
        Self {
            inner: Arc::new(Mutex::new(None)),
            registries,
            http,
            store,
            secrets,
            tts,
        }
    }

    /// Build with a caller-supplied store. Lets tests inject an
    /// in-memory store or point at a `tempfile::TempDir` without
    /// touching the real `~/.parley/sessions/`.
    pub fn with_store(
        registries: Arc<Registries>,
        http: reqwest::Client,
        store: Arc<dyn SessionStore>,
        secrets: Arc<SecretsManager>,
    ) -> Self {
        let tts = TtsRuntime {
            hub: Arc::new(TtsHub::new()),
            cache: Arc::new(FsTtsCache::new(registries.sessions_dir.clone())),
        };
        Self {
            inner: Arc::new(Mutex::new(None)),
            registries,
            http,
            store,
            secrets,
            tts,
        }
    }

    /// Build with caller-supplied store *and* TTS runtime. Lets tests
    /// share a hub/cache with a manually-driven orchestrator (rather
    /// than the one constructed by `init`).
    #[cfg(test)]
    pub fn with_store_and_tts(
        registries: Arc<Registries>,
        http: reqwest::Client,
        store: Arc<dyn SessionStore>,
        secrets: Arc<SecretsManager>,
        tts: TtsRuntime,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
            registries,
            http,
            store,
            secrets,
            tts,
        }
    }

    /// Test-only: install a pre-built orchestrator, bypassing the
    /// `init` flow's provider construction. Lets tests run the HTTP
    /// surface against a `MockProvider` without supplying real API
    /// keys.
    #[cfg(test)]
    pub async fn install_for_test(&self, orchestrator: Arc<ConversationOrchestrator>) {
        *self.inner.lock().await = Some(orchestrator);
    }
}

/// Compose the conversation routes onto a router. The caller wires
/// `state` and any additional routes / middleware.
pub fn router(state: ConversationApiState) -> Router {
    Router::new()
        .route("/conversation/init", post(init_session))
        .route("/conversation/turn", post(submit_turn))
        .route("/conversation/retry", post(retry_turn))
        .route("/conversation/discard_pending", post(discard_pending_turn))
        .route("/conversation/switch", post(switch_persona))
        .route("/conversation/snapshot", get(session_snapshot))
        .route("/conversation/save", post(save_session))
        .route("/conversation/load", post(load_session))
        .route("/conversation/sessions", get(list_sessions))
        .route("/conversation/sessions/{id}", delete(delete_session))
        .route("/conversation/tts/{turn_id}", get(stream_tts))
        .route("/conversation/tts/{turn_id}/replay", get(replay_tts))
        .route("/personas", get(list_personas))
        .route("/models", get(list_models))
        .with_state(state)
}

// ── Request / response payloads ────────────────────────────────────

/// Body for `POST /conversation/init`.
#[derive(Debug, Deserialize)]
pub struct InitRequest {
    /// Stable id for the session (callers choose; opaque to the
    /// proxy). Used to label the session in any future export.
    pub session_id: String,
    /// Persona to start the session with. Must exist in the loaded
    /// persona registry.
    pub persona_id: PersonaId,
    /// Speaker id for the AI agent's lane (e.g. `"ai-scholar"`).
    pub ai_speaker_id: SpeakerId,
    /// Display label for the AI agent.
    pub ai_speaker_label: String,
    /// Named credential to draw the provider API key from. Defaults
    /// to the `default` credential when omitted. The key itself is
    /// resolved at request time from the proxy's [`SecretsManager`]
    /// (env-var override allowed for `default`); it never crosses the
    /// wire.
    #[serde(default)]
    pub credential: Option<String>,
    /// ElevenLabs voice id for in-turn TTS synthesis. Defaults to
    /// [`DEFAULT_TTS_VOICE_ID`] when omitted.
    #[serde(default)]
    pub voice_id: Option<String>,
    /// Named credential for the ElevenLabs API key. Defaults to
    /// `default`. When the named credential is missing, the session
    /// runs in text-only mode (no TTS dispatch); the session still
    /// initializes successfully so the user can configure a key
    /// and retry via `/conversation/switch`.
    #[serde(default)]
    pub elevenlabs_credential: Option<String>,
}

/// Body for `POST /conversation/turn`.
#[derive(Debug, Deserialize)]
pub struct TurnRequest {
    /// Speaker id of the human author of the turn.
    pub speaker_id: SpeakerId,
    /// Text content of the user turn.
    pub content: String,
}

/// Body for `POST /conversation/switch`.
#[derive(Debug, Deserialize)]
pub struct SwitchRequest {
    /// Persona to make active for the next turn.
    pub persona_id: PersonaId,
    /// Model config id to pair with that persona.
    pub model_config_id: ModelConfigId,
    /// Named credential to draw the provider API key from. Defaults
    /// to `default` when omitted. Same resolution rules as
    /// [`InitRequest::credential`].
    #[serde(default)]
    pub credential: Option<String>,
}

/// Body for `POST /conversation/load`.
///
/// Credentials are *not* persisted with the session. Loading
/// re-resolves the provider key from the proxy's [`SecretsManager`]
/// using the optional `credential` field (defaults to `default`).
/// Same provider-construction rules as `/init`: only Anthropic is
/// wired today.
#[derive(Debug, Deserialize)]
pub struct LoadRequest {
    /// Id of a previously saved session.
    pub session_id: String,
    /// Named credential to use when reconstructing the provider for
    /// the loaded session's active model. Defaults to `default`.
    #[serde(default)]
    pub credential: Option<String>,
    /// ElevenLabs voice id to use for synthesis after load. Defaults
    /// to [`DEFAULT_TTS_VOICE_ID`].
    #[serde(default)]
    pub voice_id: Option<String>,
    /// Named credential for the ElevenLabs API key. Defaults to
    /// `default`. Missing-credential behavior matches `/init`:
    /// load succeeds, TTS is disabled until a credential is
    /// configured.
    #[serde(default)]
    pub elevenlabs_credential: Option<String>,
}

/// Compact summary of a saved session, returned by `GET
/// /conversation/sessions`. The `title` is derived best-effort from
/// the first user turn so the picker UI can render something more
/// human-friendly than the raw filename id.
#[derive(Debug, Serialize)]
pub struct SessionSummary {
    /// Filesystem id (also the filename stem).
    pub id: String,
    /// Truncated preview of the first user turn, or an empty string
    /// if the session has no user turns yet (or failed to load).
    pub title: String,
}

/// Response body for `GET /conversation/sessions`.
#[derive(Debug, Serialize)]
pub struct SessionList {
    /// All saved sessions, sorted by id (lexicographic, which for
    /// our `sess-{epoch_ms}-{rand}` ids is also chronological).
    pub sessions: Vec<SessionSummary>,
}

/// Compact view of a [`Persona`] used by the picker UI. Strips the
/// system-prompt body and tier internals — those aren't useful for
/// rendering a dropdown and may be large.
#[derive(Debug, Serialize)]
pub struct PersonaSummary {
    /// Stable persona id.
    pub id: PersonaId,
    /// Display name.
    pub name: String,
    /// Free-form description for tooltips.
    pub description: String,
}

/// Compact view of a [`ModelConfig`] used by the picker UI.
#[derive(Debug, Serialize)]
pub struct ModelSummary {
    /// Stable model-config id.
    pub id: ModelConfigId,
    /// Provider tag (anthropic, openai, ...). Lets the client warn
    /// before submitting credentials of the wrong shape.
    pub provider: LlmProviderTag,
    /// Provider-specific model name (e.g. `claude-opus-4-7-...`).
    pub model_name: String,
    /// Total context window in tokens.
    pub context_window: u32,
}

/// Response body for `GET /personas`.
#[derive(Debug, Serialize)]
pub struct PersonaListResponse {
    /// Personas sorted by id.
    pub personas: Vec<PersonaSummary>,
}

/// Response body for `GET /models`.
#[derive(Debug, Serialize)]
pub struct ModelListResponse {
    /// Model configs sorted by id.
    pub models: Vec<ModelSummary>,
}

/// Response body for `POST /conversation/save`.
#[derive(Debug, Serialize)]
pub struct SaveResponse {
    /// Id of the saved session (echoed back for client convenience).
    pub session_id: String,
}

/// Wire-format error body. Stable shape across all routes.
#[derive(Debug, Serialize)]
pub struct ErrorBody {
    /// Human-readable error message.
    pub error: String,
}

impl ErrorBody {
    fn new(message: impl Into<String>) -> Self {
        Self {
            error: message.into(),
        }
    }
}

// ── Handlers ───────────────────────────────────────────────────────

async fn init_session(
    State(state): State<ConversationApiState>,
    Json(req): Json<InitRequest>,
) -> Result<Json<ConversationSession>, (StatusCode, Json<ErrorBody>)> {
    // Resolve persona + model from the loaded registries.
    let persona = state
        .registries
        .personas
        .get(&req.persona_id)
        .cloned()
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorBody::new(format!(
                    "unknown persona '{}'",
                    req.persona_id
                ))),
            )
        })?;
    let model_id = persona.tiers.heavy.model_config.clone();
    let model = state
        .registries
        .models
        .get(&model_id)
        .cloned()
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorBody::new(format!(
                    "persona '{}' references unknown model '{}'",
                    req.persona_id, model_id
                ))),
            )
        })?;

    // Build the provider for this model. Only Anthropic is wired in
    // this slice; other tags surface as 501.
    let provider = build_provider(
        &model,
        req.credential.as_deref(),
        &state.http,
        &state.secrets,
    )?;

    let session = ConversationSession::new(
        req.session_id,
        Speaker::ai_agent(req.ai_speaker_id, &req.ai_speaker_label),
        persona.id.clone(),
        model.id.clone(),
    );
    let snapshot = session.clone();
    let (tts, tts_voice_id) = resolve_tts(
        &state,
        req.voice_id.as_deref(),
        req.elevenlabs_credential.as_deref(),
    );
    let ctx = OrchestratorContext {
        personas: state.registries.personas.clone(),
        models: state.registries.models.clone(),
        providers: HashMap::from([(model.id.clone(), provider)]),
        prompts_dir: state.registries.prompts_dir.clone(),
        tts,
        tts_cache: Some(state.tts.cache.clone()),
        tts_hub: Some(state.tts.hub.clone()),
        tts_voice_id,
    };
    let orchestrator = Arc::new(ConversationOrchestrator::new(session, ctx));
    *state.inner.lock().await = Some(orchestrator);
    Ok(Json(snapshot))
}

async fn submit_turn(
    State(state): State<ConversationApiState>,
    Json(req): Json<TurnRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, Json<ErrorBody>)> {
    let orchestrator = require_session(&state).await?;

    // Drive the orchestrator. Pre-dispatch errors (unknown persona
    // etc.) come back synchronously and become a 4xx; mid-stream
    // errors are reported as `OrchestratorEvent::Failed` inside the
    // SSE stream and the stream still ends cleanly.
    let event_stream = orchestrator
        .submit_user_turn(req.speaker_id, req.content)
        .await
        .map_err(orchestrator_error_to_response)?;

    let sse_stream = event_stream.map(|event| {
        let json = serde_json::to_string(&event).expect("OrchestratorEvent serialization");
        Ok::<_, Infallible>(Event::default().event(event_name(&event)).data(json))
    });
    Ok(Sse::new(sse_stream).keep_alive(KeepAlive::default()))
}

/// `POST /conversation/retry` — re-dispatch the session's pending
/// tail user turn (left behind by a failed `submit_turn`). Streams
/// SSE events identical to `/conversation/turn` minus the synthetic
/// `user_turn_appended` (the user turn is already in the session).
/// Returns 409 when there is no pending turn to retry.
async fn retry_turn(
    State(state): State<ConversationApiState>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, Json<ErrorBody>)> {
    let orchestrator = require_session(&state).await?;
    let event_stream = orchestrator
        .retry_pending()
        .await
        .map_err(orchestrator_error_to_response)?;
    let sse_stream = event_stream.map(|event| {
        let json = serde_json::to_string(&event).expect("OrchestratorEvent serialization");
        Ok::<_, Infallible>(Event::default().event(event_name(&event)).data(json))
    });
    Ok(Sse::new(sse_stream).keep_alive(KeepAlive::default()))
}

/// `POST /conversation/discard_pending` — pop the trailing
/// pending user turn from the session so subsequent dispatches see
/// clean history. Used by the Dismiss path after a failure. Returns
/// 204 on success, 409 when there is nothing pending.
async fn discard_pending_turn(
    State(state): State<ConversationApiState>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    let orchestrator = require_session(&state).await?;
    orchestrator
        .discard_pending()
        .await
        .map_err(orchestrator_error_to_response)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn switch_persona(
    State(state): State<ConversationApiState>,
    Json(req): Json<SwitchRequest>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    let orchestrator = require_session(&state).await?;
    if !state.registries.personas.contains_key(&req.persona_id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorBody::new(format!(
                "unknown persona '{}'",
                req.persona_id
            ))),
        ));
    }
    let model = state
        .registries
        .models
        .get(&req.model_config_id)
        .cloned()
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorBody::new(format!(
                    "unknown model '{}'",
                    req.model_config_id
                ))),
            )
        })?;

    // Snapshot current session, then rebuild the orchestrator with a
    // freshly constructed provider for the new model. This is how we
    // keep `OrchestratorContext::providers` aligned with the active
    // model without making the context internally mutable. The
    // session itself — turns, speakers, persona history — carries
    // over unchanged; `switch_persona` then records the activation
    // so the next turn dispatches against the new pair.
    let mut snapshot = orchestrator.session_snapshot().await;
    let provider = build_provider(
        &model,
        req.credential.as_deref(),
        &state.http,
        &state.secrets,
    )?;
    snapshot.switch_persona(req.persona_id.clone(), req.model_config_id.clone());
    // `switch` doesn't carry voice/elevenlabs fields today — reuse
    // the default credential resolution (and the default voice).
    // The browser can re-init with explicit values to change voice.
    let (tts, tts_voice_id) = resolve_tts(&state, None, None);
    let ctx = OrchestratorContext {
        personas: state.registries.personas.clone(),
        models: state.registries.models.clone(),
        providers: HashMap::from([(model.id.clone(), provider)]),
        prompts_dir: state.registries.prompts_dir.clone(),
        tts,
        tts_cache: Some(state.tts.cache.clone()),
        tts_hub: Some(state.tts.hub.clone()),
        tts_voice_id,
    };
    let new_orchestrator = Arc::new(ConversationOrchestrator::new(snapshot, ctx));
    *state.inner.lock().await = Some(new_orchestrator);
    Ok(StatusCode::NO_CONTENT)
}

async fn session_snapshot(
    State(state): State<ConversationApiState>,
) -> Result<Json<ConversationSession>, (StatusCode, Json<ErrorBody>)> {
    let orchestrator = require_session(&state).await?;
    Ok(Json(orchestrator.session_snapshot().await))
}

async fn save_session(
    State(state): State<ConversationApiState>,
) -> Result<Json<SaveResponse>, (StatusCode, Json<ErrorBody>)> {
    let orchestrator = require_session(&state).await?;
    let snapshot = orchestrator.session_snapshot().await;
    let id = snapshot.id.clone();
    state
        .store
        .save(&snapshot)
        .await
        .map_err(session_store_error_to_response)?;
    Ok(Json(SaveResponse { session_id: id }))
}

async fn load_session(
    State(state): State<ConversationApiState>,
    Json(req): Json<LoadRequest>,
) -> Result<Json<ConversationSession>, (StatusCode, Json<ErrorBody>)> {
    let session = state
        .store
        .load(&req.session_id)
        .await
        .map_err(session_store_error_to_response)?;

    // Resolve the active persona/model from the registries against
    // what the file says is active. Drift (a persona that has since
    // been deleted from disk) is a 422 — the file is fine, but the
    // current registries can no longer drive it.
    let active = session.active().clone();
    // Verify the saved persona still exists in the live registry.
    // The value itself is unused here (the orchestrator looks it up
    // again at dispatch time), but the lookup must happen so we can
    // surface drift as 422 rather than crashing on a missing key
    // later.
    let _persona = state
        .registries
        .personas
        .get(&active.persona_id)
        .cloned()
        .ok_or_else(|| {
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ErrorBody::new(format!(
                    "saved session uses persona '{}' which is no longer in the registry",
                    active.persona_id
                ))),
            )
        })?;
    let model = state
        .registries
        .models
        .get(&active.model_config_id)
        .cloned()
        .ok_or_else(|| {
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ErrorBody::new(format!(
                    "saved session uses model '{}' which is no longer in the registry",
                    active.model_config_id
                ))),
            )
        })?;

    let provider = build_provider(
        &model,
        req.credential.as_deref(),
        &state.http,
        &state.secrets,
    )?;

    let snapshot = session.clone();
    let (tts, tts_voice_id) = resolve_tts(
        &state,
        req.voice_id.as_deref(),
        req.elevenlabs_credential.as_deref(),
    );
    let ctx = OrchestratorContext {
        personas: state.registries.personas.clone(),
        models: state.registries.models.clone(),
        providers: HashMap::from([(model.id.clone(), provider)]),
        prompts_dir: state.registries.prompts_dir.clone(),
        tts,
        tts_cache: Some(state.tts.cache.clone()),
        tts_hub: Some(state.tts.hub.clone()),
        tts_voice_id,
    };
    let orchestrator = Arc::new(ConversationOrchestrator::new(session, ctx));
    *state.inner.lock().await = Some(orchestrator);
    Ok(Json(snapshot))
}

async fn list_sessions(
    State(state): State<ConversationApiState>,
) -> Result<Json<SessionList>, (StatusCode, Json<ErrorBody>)> {
    let mut ids = state
        .store
        .list()
        .await
        .map_err(session_store_error_to_response)?;
    ids.sort();
    // Best-effort title derivation: load each session and pull the
    // first user turn. A load failure (corrupt JSON, registry drift)
    // is non-fatal — we still return the id with an empty title so
    // the picker can show *something*.
    let mut sessions = Vec::with_capacity(ids.len());
    for id in ids {
        let title = match state.store.load(&id).await {
            Ok(session) => derive_session_title(&session),
            Err(_) => String::new(),
        };
        sessions.push(SessionSummary { id, title });
    }
    Ok(Json(SessionList { sessions }))
}

/// Pull a short, human-friendly title out of a session: the first
/// user turn's content, collapsed to a single line and truncated to
/// roughly 60 chars at a char boundary. Returns an empty string when
/// there are no user turns.
fn derive_session_title(session: &ConversationSession) -> String {
    const MAX_CHARS: usize = 60;
    let Some(first_user) = session
        .turns
        .iter()
        .find(|t| matches!(t.role, parley_core::chat::ChatRole::User))
    else {
        return String::new();
    };
    let collapsed: String = first_user
        .content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.chars().count() <= MAX_CHARS {
        collapsed
    } else {
        let truncated: String = collapsed.chars().take(MAX_CHARS).collect();
        format!("{truncated}…")
    }
}

/// `DELETE /conversation/sessions/{id}` — remove a saved session
/// from disk. Returns `204 No Content` on success, `404 Not Found`
/// for unknown ids, `400 Bad Request` for invalid ids. Does not
/// touch the in-memory active session: deleting the file does not
/// detach an orchestrator that's already loaded that id.
async fn delete_session(
    State(state): State<ConversationApiState>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    state
        .store
        .delete(&id)
        .await
        .map_err(session_store_error_to_response)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_personas(State(state): State<ConversationApiState>) -> Json<PersonaListResponse> {
    let mut personas: Vec<PersonaSummary> = state
        .registries
        .personas
        .values()
        .map(|p| PersonaSummary {
            id: p.id.clone(),
            name: p.name.clone(),
            description: p.description.clone(),
        })
        .collect();
    personas.sort_by(|a, b| a.id.cmp(&b.id));
    Json(PersonaListResponse { personas })
}

async fn list_models(State(state): State<ConversationApiState>) -> Json<ModelListResponse> {
    let mut models: Vec<ModelSummary> = state
        .registries
        .models
        .values()
        .map(|m| ModelSummary {
            id: m.id.clone(),
            provider: m.provider,
            model_name: m.model_name.clone(),
            context_window: m.context_window,
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Json(ModelListResponse { models })
}

// ── Helpers ────────────────────────────────────────────────────────

async fn require_session(
    state: &ConversationApiState,
) -> Result<Arc<ConversationOrchestrator>, (StatusCode, Json<ErrorBody>)> {
    state.inner.lock().await.clone().ok_or((
        StatusCode::CONFLICT,
        Json(ErrorBody::new(
            "no active session — POST /conversation/init first",
        )),
    ))
}

fn build_provider(
    model: &ModelConfig,
    credential: Option<&str>,
    http: &reqwest::Client,
    secrets: &SecretsManager,
) -> Result<Arc<dyn LlmProvider>, (StatusCode, Json<ErrorBody>)> {
    match model.provider {
        LlmProviderTag::Anthropic => {
            let credential = credential.unwrap_or(DEFAULT_CREDENTIAL);
            let key = match secrets.resolve(ProviderId::Anthropic, credential) {
                Some(k) => k,
                None => {
                    return Err((
                        StatusCode::PRECONDITION_FAILED,
                        Json(ErrorBody::new(format!(
                            "no Anthropic credential '{credential}' configured—set one via /api/secrets first",
                        ))),
                    ));
                }
            };
            let id = format!("anthropic:{}", model.model_name);
            Ok(Arc::new(AnthropicLlm::new(
                id,
                model.model_name.clone(),
                key,
                model.context_window,
                model.rates,
                http.clone(),
            )))
        }
        other => Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(ErrorBody::new(format!(
                "provider {other:?} is not wired into this proxy yet"
            ))),
        )),
    }
}

/// Map [`OrchestratorError`] (pre-dispatch failures) to an HTTP
/// response. Distinguishes "your config is wrong" (4xx) from "the
/// proxy is misconfigured" (5xx) where we can.
fn orchestrator_error_to_response(err: OrchestratorError) -> (StatusCode, Json<ErrorBody>) {
    let status = match &err {
        OrchestratorError::UnknownPersona(_) | OrchestratorError::UnknownModelConfig(_) => {
            StatusCode::BAD_REQUEST
        }
        OrchestratorError::NoProvider(_) | OrchestratorError::PromptFile { .. } => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        // 409: the session state isn't compatible with the request
        // (e.g. retry/discard called when there's nothing pending).
        OrchestratorError::NoPendingTurn => StatusCode::CONFLICT,
    };
    (status, Json(ErrorBody::new(err.to_string())))
}

/// Map [`SessionStoreError`] to an HTTP response. `InvalidId` is a
/// client error; `NotFound` maps to 404; everything else (I/O,
/// decode) is server-side.
fn session_store_error_to_response(err: SessionStoreError) -> (StatusCode, Json<ErrorBody>) {
    let status = match &err {
        SessionStoreError::InvalidId(_) => StatusCode::BAD_REQUEST,
        SessionStoreError::NotFound(_) => StatusCode::NOT_FOUND,
        SessionStoreError::Io { .. } | SessionStoreError::Decode { .. } => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    (status, Json(ErrorBody::new(err.to_string())))
}

/// Resolve TTS provider + voice for a session-construction request.
///
/// Returns `(provider, voice_id)` when an ElevenLabs key is
/// configured for `credential` (defaulting to `default`); both are
/// `None` when no key is configured. The session still constructs
/// successfully on a missing key — TTS dispatch simply no-ops and
/// the session runs in text-only mode (spec §6 "graceful
/// degradation").
fn resolve_tts(
    state: &ConversationApiState,
    voice_id: Option<&str>,
    credential: Option<&str>,
) -> (Option<Arc<dyn crate::tts::TtsProvider>>, Option<String>) {
    let credential = credential.unwrap_or(DEFAULT_CREDENTIAL);
    let key = match state.secrets.resolve(ProviderId::ElevenLabs, credential) {
        Some(k) => k,
        None => return (None, None),
    };
    let voice = voice_id.unwrap_or(DEFAULT_TTS_VOICE_ID).to_string();
    let provider: Arc<dyn crate::tts::TtsProvider> =
        Arc::new(ElevenLabsTts::new(key, state.http.clone()));
    (Some(provider), Some(voice))
}

/// `GET /conversation/tts/{turn_id}` — SSE stream of MP3 audio for
/// a turn. Combines a snapshot of the cache file (if any) with a
/// live broadcast subscription (if any) so subscribers that connect
/// mid-flight see every byte exactly once.
///
/// Cache and broadcast are subscribed to in this order:
///
/// 1. Subscribe to the live hub (so we don't miss frames during
///    the cache read).
/// 2. Snapshot cache bytes — the orchestrator always writes to the
///    cache *before* it broadcasts, so cache length ≥ any byte
///    count a queued frame can carry.
/// 3. Emit the cache snapshot as a single `audio` frame.
/// 4. Drain the live receiver, dropping any `Audio` frame whose
///    cumulative `total_bytes_after` ≤ cache snapshot length (those
///    bytes are already in the snapshot we sent).
///
/// Returns `404` when the turn id has neither a live broadcast nor
/// a cache file. The session id is read from the active
/// orchestrator; routes that fire before `init` get `409`.
async fn stream_tts(
    State(state): State<ConversationApiState>,
    Path(turn_id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, Json<ErrorBody>)> {
    let orchestrator = require_session(&state).await?;
    let session_id = orchestrator.session_snapshot().await.id;

    // Step 1: subscribe FIRST so frames published while we read
    // the cache aren't lost. `None` means no live broadcast.
    let live_rx = state.tts.hub.subscribe(&turn_id);

    // Step 2: snapshot cache (best-effort). `None` means no file.
    let cache_bytes: Option<Vec<u8>> = match state.tts.cache.reader(&session_id, &turn_id).await {
        Ok(Some(reader)) => Some(reader.into_bytes()),
        Ok(None) => None,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody::new(format!("tts cache read failed: {e}"))),
            ));
        }
    };
    if live_rx.is_none() && cache_bytes.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorBody::new(format!(
                "no tts stream or cache for turn '{turn_id}'"
            ))),
        ));
    }

    let cache_len = cache_bytes.as_ref().map(|b| b.len() as u64).unwrap_or(0);
    let stream = async_stream::stream! {
        // (a) Cache prefix as a single audio frame.
        if let Some(bytes) = cache_bytes
            && !bytes.is_empty()
        {
            yield Ok::<_, Infallible>(audio_event(&bytes));
        }

        // (b) Live tail. Skip frames already covered by the cache
        // snapshot (their `total_bytes_after` ≤ cache_len).
        if let Some(mut rx) = live_rx {
            loop {
                match rx.recv().await {
                    Ok(TtsBroadcastFrame::Audio { bytes, total_bytes_after }) => {
                        if total_bytes_after <= cache_len {
                            continue;
                        }
                        yield Ok(audio_event(&bytes));
                    }
                    Ok(TtsBroadcastFrame::Done) => {
                        yield Ok(done_event());
                        break;
                    }
                    Ok(TtsBroadcastFrame::Error(message)) => {
                        yield Ok(error_event(&message));
                        break;
                    }
                    // Channel closed without a terminal frame
                    // (sender dropped without finish/fail). Treat
                    // as a normal completion — the cache prefix
                    // covers everything that was actually emitted.
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        yield Ok(done_event());
                        break;
                    }
                    // Subscriber lagged — broadcaster outran us.
                    // Skip the lost frames; later frames carry
                    // their own `total_bytes_after` so the next
                    // valid Audio still slots in correctly.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        } else {
            // Cache-only: emit `done` immediately after the snapshot.
            yield Ok(done_event());
        }
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// `GET /conversation/tts/{turn_id}/replay` — return the cached MP3
/// file as a single response with `Content-Type: audio/mpeg`,
/// suitable for `<audio src="..."/>`. Returns `404` when no cache
/// file exists; `409` when no session is active.
async fn replay_tts(
    State(state): State<ConversationApiState>,
    Path(turn_id): Path<String>,
) -> Result<Response, (StatusCode, Json<ErrorBody>)> {
    let orchestrator = require_session(&state).await?;
    let session_id = orchestrator.session_snapshot().await.id;
    let reader = state
        .tts
        .cache
        .reader(&session_id, &turn_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody::new(format!("tts cache read failed: {e}"))),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorBody::new(format!("no tts cache for turn '{turn_id}'"))),
            )
        })?;
    let bytes = reader.into_bytes();
    let mut resp = (StatusCode::OK, bytes).into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, "audio/mpeg".parse().unwrap());
    Ok(resp)
}

/// Build a single `audio` SSE frame from a chunk of MP3 bytes.
fn audio_event(bytes: &[u8]) -> Event {
    let payload = serde_json::json!({
        "type": "audio",
        "b64": BASE64_STANDARD.encode(bytes),
    });
    Event::default().event("audio").data(payload.to_string())
}

/// Terminal `done` SSE frame.
fn done_event() -> Event {
    Event::default().event("done").data(r#"{"type":"done"}"#)
}

/// Terminal `error` SSE frame with a human-readable message.
fn error_event(message: &str) -> Event {
    let payload = serde_json::json!({
        "type": "error",
        "message": message,
    });
    Event::default().event("error").data(payload.to_string())
}

/// Stable `event:` field name for SSE frames. Mirrors the
/// `#[serde(tag = "type")]` discriminant on [`OrchestratorEvent`] so
/// clients can dispatch on either the SSE event name or the
/// `"type"` field of the JSON payload.
fn event_name(event: &OrchestratorEvent) -> &'static str {
    match event {
        OrchestratorEvent::StateChanged { .. } => "state_changed",
        OrchestratorEvent::UserTurnAppended { .. } => "user_turn_appended",
        OrchestratorEvent::Token { .. } => "token",
        OrchestratorEvent::AiTurnAppended { .. } => "ai_turn_appended",
        OrchestratorEvent::TtsStarted { .. } => "tts_started",
        OrchestratorEvent::TtsSentenceDone { .. } => "tts_sentence_done",
        OrchestratorEvent::TtsFinished { .. } => "tts_finished",
        OrchestratorEvent::Failed { .. } => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::test_support::{MockItem, MockProvider};
    use crate::orchestrator::{ConversationOrchestrator, OrchestratorContext};
    use crate::secrets::{InMemoryKeyStore, StaticEnv};
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request};
    use parley_core::chat::TokenUsage;
    use parley_core::model_config::{LlmProviderTag, TokenRates};
    use parley_core::persona::{
        PersonaContextSettings, PersonaTier, PersonaTiers, PersonaTtsSettings, SystemPrompt,
    };
    use tower::ServiceExt;

    // ── Fixtures ───────────────────────────────────────────────

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

    fn registries_with(persona: Persona, model: ModelConfig) -> Arc<Registries> {
        Arc::new(Registries {
            personas: [(persona.id.clone(), persona)].into(),
            models: [(model.id.clone(), model)].into(),
            prompts_dir: PathBuf::from("/nonexistent"),
            sessions_dir: PathBuf::from("/nonexistent-sessions"),
        })
    }

    async fn install_orchestrator(
        state: &ConversationApiState,
        persona: Persona,
        model: ModelConfig,
        provider: Arc<dyn LlmProvider>,
    ) -> Arc<ConversationOrchestrator> {
        let session = ConversationSession::new(
            "sess-test",
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
        };
        let orch = Arc::new(ConversationOrchestrator::new(session, ctx));
        state.install_for_test(orch.clone()).await;
        orch
    }

    fn build_state(persona: Persona, model: ModelConfig) -> ConversationApiState {
        ConversationApiState::new(
            registries_with(persona, model),
            reqwest::Client::new(),
            test_secrets_with_default("sk-test"),
        )
    }

    /// Construct an in-memory [`SecretsManager`] that already has an
    /// Anthropic `default` credential present. Used by the
    /// happy-path tests that drive `init`/`switch`/`load` and need
    /// `build_provider` to resolve a key.
    fn test_secrets_with_default(key: &str) -> Arc<SecretsManager> {
        let store = Box::new(InMemoryKeyStore::new());
        let env = Box::new(StaticEnv::new());
        let tmp = tempfile::TempDir::new().unwrap();
        let mgr = SecretsManager::new(store, env, tmp.path().join("credentials.json"));
        mgr.set(ProviderId::Anthropic, DEFAULT_CREDENTIAL, key)
            .unwrap();
        // Leak the tempdir handle so the index file outlives the
        // builder. Tests are short-lived; the OS reclaims on exit.
        std::mem::forget(tmp);
        Arc::new(mgr)
    }

    /// Construct an empty in-memory [`SecretsManager`] \u2014 use this
    /// to verify `build_provider` returns 412 when no credential is
    /// configured.
    fn empty_test_secrets() -> Arc<SecretsManager> {
        let store = Box::new(InMemoryKeyStore::new());
        let env = Box::new(StaticEnv::new());
        let tmp = tempfile::TempDir::new().unwrap();
        let mgr = SecretsManager::new(store, env, tmp.path().join("credentials.json"));
        std::mem::forget(tmp);
        Arc::new(mgr)
    }

    /// Drain the body of a 200 response into a String.
    async fn read_body(resp: axum::response::Response) -> String {
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// Parse an SSE body into a sequence of `(event, data)` pairs.
    /// Blank-line-separated frames; `event:` and `data:` fields only
    /// (no multi-line data, no comments — we never emit them).
    fn parse_sse(body: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for frame in body.split("\n\n") {
            let frame = frame.trim();
            if frame.is_empty() {
                continue;
            }
            let mut event = String::new();
            let mut data = String::new();
            for line in frame.lines() {
                if let Some(rest) = line.strip_prefix("event:") {
                    event = rest.trim().to_string();
                } else if let Some(rest) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(rest.trim_start());
                }
            }
            if !event.is_empty() || !data.is_empty() {
                out.push((event, data));
            }
        }
        out
    }

    // ── Tests ──────────────────────────────────────────────────

    #[tokio::test]
    async fn snapshot_404s_until_init() {
        let state = build_state(sample_persona("scholar", "m1", "x"), sample_model("m1"));
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/conversation/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn turn_endpoint_streams_orchestrator_events_as_sse() {
        let persona = sample_persona("scholar", "m1", "be helpful");
        let model = sample_model("m1");
        let state = build_state(persona.clone(), model.clone());

        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(
            "mock",
            vec![
                MockItem::Text("Hello, ".into()),
                MockItem::Text("world.".into()),
            ],
            TokenUsage {
                input: 10,
                output: 4,
            },
        ));
        install_orchestrator(&state, persona, model, provider).await;

        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/conversation/turn")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "speaker_id": "gavin",
                            "content": "hi",
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp).await;
        let frames = parse_sse(&body);

        // Pull the event names in order.
        let names: Vec<&str> = frames.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"user_turn_appended"));
        assert!(names.contains(&"state_changed"));
        assert!(names.contains(&"token"));
        assert!(names.contains(&"ai_turn_appended"));

        // Token deltas concatenate to the full assistant text.
        let text: String = frames
            .iter()
            .filter(|(n, _)| n == "token")
            .map(|(_, d)| {
                let v: serde_json::Value = serde_json::from_str(d).unwrap();
                v["delta"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(text, "Hello, world.");
    }

    #[tokio::test]
    async fn turn_endpoint_streams_failed_event_on_provider_error() {
        let persona = sample_persona("scholar", "m1", "x");
        let model = sample_model("m1");
        let state = build_state(persona.clone(), model.clone());

        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(
            "mock",
            vec![MockItem::Err(crate::llm::LlmError::Other("boom".into()))],
            TokenUsage::default(),
        ));
        install_orchestrator(&state, persona, model, provider).await;

        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/conversation/turn")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"speaker_id":"g","content":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp).await;
        let frames = parse_sse(&body);
        let names: Vec<&str> = frames.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"failed"));
        assert!(!names.contains(&"ai_turn_appended"));
    }

    #[tokio::test]
    async fn retry_endpoint_redispatches_pending_user_turn() {
        let persona = sample_persona("scholar", "m1", "x");
        let model = sample_model("m1");
        let state = build_state(persona.clone(), model.clone());

        // First call errors out and leaves the user turn pending.
        // Second call must succeed using the *same* tail user turn,
        // without appending a duplicate.
        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(
            "mock",
            vec![
                MockItem::Err(crate::llm::LlmError::Other("transient".into())),
                MockItem::Text("recovered".into()),
            ],
            TokenUsage::default(),
        ));
        install_orchestrator(&state, persona, model, provider).await;

        // Initial /turn — should fail mid-stream.
        let resp = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/conversation/turn")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"speaker_id":"g","content":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let _ = read_body(resp).await;

        // Retry — must succeed, must NOT emit user_turn_appended,
        // must end with ai_turn_appended.
        let resp = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/conversation/retry")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp).await;
        let frames = parse_sse(&body);
        let names: Vec<&str> = frames.iter().map(|(n, _)| n.as_str()).collect();
        assert!(!names.contains(&"user_turn_appended"));
        assert!(names.contains(&"ai_turn_appended"));

        // Snapshot must show exactly one user turn + one assistant.
        let snap = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/conversation/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&read_body(snap).await).unwrap();
        let turns = v["turns"].as_array().unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0]["role"], "user");
        assert_eq!(turns[1]["role"], "assistant");
    }

    #[tokio::test]
    async fn retry_endpoint_returns_409_when_no_pending_turn() {
        let persona = sample_persona("scholar", "m1", "x");
        let model = sample_model("m1");
        let state = build_state(persona.clone(), model.clone());
        let provider: Arc<dyn LlmProvider> =
            Arc::new(MockProvider::new("mock", vec![], TokenUsage::default()));
        install_orchestrator(&state, persona, model, provider).await;

        let resp = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/conversation/retry")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn discard_pending_endpoint_pops_orphan_user_turn() {
        let persona = sample_persona("scholar", "m1", "x");
        let model = sample_model("m1");
        let state = build_state(persona.clone(), model.clone());
        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(
            "mock",
            vec![MockItem::Err(crate::llm::LlmError::Other("boom".into()))],
            TokenUsage::default(),
        ));
        install_orchestrator(&state, persona, model, provider).await;

        // Force the orphan tail.
        let resp = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/conversation/turn")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"speaker_id":"g","content":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let _ = read_body(resp).await;

        let resp = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/conversation/discard_pending")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let snap = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/conversation/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&read_body(snap).await).unwrap();
        assert_eq!(v["turns"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn discard_pending_endpoint_returns_409_when_clean() {
        let persona = sample_persona("scholar", "m1", "x");
        let model = sample_model("m1");
        let state = build_state(persona.clone(), model.clone());
        let provider: Arc<dyn LlmProvider> =
            Arc::new(MockProvider::new("mock", vec![], TokenUsage::default()));
        install_orchestrator(&state, persona, model, provider).await;

        let resp = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/conversation/discard_pending")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn snapshot_returns_session_after_turn() {
        let persona = sample_persona("scholar", "m1", "x");
        let model = sample_model("m1");
        let state = build_state(persona.clone(), model.clone());
        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(
            "mock",
            vec![MockItem::Text("ok".into())],
            TokenUsage::default(),
        ));
        install_orchestrator(&state, persona, model, provider).await;

        // Run one turn so a snapshot has something to show. We must
        // drain the SSE body — the orchestrator's stream only
        // appends the AI turn as a side effect of being polled.
        let turn_resp = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/conversation/turn")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"speaker_id":"g","content":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let _ = read_body(turn_resp).await;

        let resp = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/conversation/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["id"], "sess-test");
        let turns = v["turns"].as_array().unwrap();
        assert_eq!(turns.len(), 2);
    }

    #[tokio::test]
    async fn switch_endpoint_records_persona_change() {
        let persona = sample_persona("scholar", "m1", "x");
        let other = sample_persona("storyteller", "m1", "x");
        let model = sample_model("m1");
        let registries = Arc::new(Registries {
            personas: [
                (persona.id.clone(), persona.clone()),
                (other.id.clone(), other.clone()),
            ]
            .into(),
            models: [(model.id.clone(), model.clone())].into(),
            prompts_dir: PathBuf::from("/x"),
            sessions_dir: PathBuf::from("/x-sessions"),
        });
        let state = ConversationApiState::new(
            registries,
            reqwest::Client::new(),
            test_secrets_with_default("sk-test"),
        );
        let provider: Arc<dyn LlmProvider> =
            Arc::new(MockProvider::new("mock", vec![], TokenUsage::default()));
        install_orchestrator(&state, persona, model.clone(), provider).await;

        let resp = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/conversation/switch")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "persona_id": "storyteller",
                            "model_config_id": "m1",
                            "anthropic_key": "test-key",
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Snapshot reflects the new active persona via persona_history.
        let snap = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/conversation/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&read_body(snap).await).unwrap();
        let history = v["persona_history"].as_array().unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1]["persona_id"], "storyteller");
    }

    #[tokio::test]
    async fn switch_with_unknown_persona_is_400() {
        let persona = sample_persona("scholar", "m1", "x");
        let model = sample_model("m1");
        let state = build_state(persona.clone(), model.clone());
        let provider: Arc<dyn LlmProvider> =
            Arc::new(MockProvider::new("mock", vec![], TokenUsage::default()));
        install_orchestrator(&state, persona, model, provider).await;

        let resp = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/conversation/switch")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"persona_id":"ghost","model_config_id":"m1","anthropic_key":"k"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn init_with_unknown_persona_is_400() {
        let state = build_state(sample_persona("scholar", "m1", "x"), sample_model("m1"));
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/conversation/init")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "session_id": "s",
                            "persona_id": "nope",
                            "ai_speaker_id": "ai-x",
                            "ai_speaker_label": "X",
                            "anthropic_key": "k",
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn init_anthropic_without_configured_credential_is_412() {
        // build_state seeds a `default` Anthropic credential, so use
        // an explicit empty manager to verify the missing-credential
        // path returns `412 Precondition Failed` per the secrets
        // storage spec §6.
        let state = ConversationApiState::new(
            registries_with(sample_persona("scholar", "m1", "x"), sample_model("m1")),
            reqwest::Client::new(),
            empty_test_secrets(),
        );
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/conversation/init")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "session_id": "s",
                            "persona_id": "scholar",
                            "ai_speaker_id": "ai-x",
                            "ai_speaker_label": "X",
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test]
    async fn init_with_anthropic_key_creates_session() {
        let state = build_state(sample_persona("scholar", "m1", "x"), sample_model("m1"));
        let app = router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/conversation/init")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "session_id": "sess-real",
                            "persona_id": "scholar",
                            "ai_speaker_id": "ai-scholar",
                            "ai_speaker_label": "Scholar",
                            "anthropic_key": "sk-test",
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["id"], "sess-real");
        // Session is now installed.
        let snap = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/conversation/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(snap.status(), StatusCode::OK);
    }

    // ── Persistence ────────────────────────────────────────────

    /// Build a state whose store writes into the supplied tempdir.
    fn build_state_with_store_dir(
        persona: Persona,
        model: ModelConfig,
        store_dir: PathBuf,
    ) -> ConversationApiState {
        let registries = Arc::new(Registries {
            personas: [(persona.id.clone(), persona)].into(),
            models: [(model.id.clone(), model)].into(),
            prompts_dir: PathBuf::from("/nonexistent"),
            sessions_dir: store_dir.clone(),
        });
        let store: Arc<dyn SessionStore> = Arc::new(FsSessionStore::new(store_dir));
        ConversationApiState::with_store(
            registries,
            reqwest::Client::new(),
            store,
            test_secrets_with_default("sk-test"),
        )
    }

    #[tokio::test]
    async fn save_endpoint_writes_session_and_load_restores_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let persona = sample_persona("scholar", "m1", "x");
        let model = sample_model("m1");

        // First state: run a turn, then save.
        let state_a =
            build_state_with_store_dir(persona.clone(), model.clone(), tmp.path().to_path_buf());
        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(
            "mock",
            vec![MockItem::Text("hi back".into())],
            TokenUsage::default(),
        ));
        install_orchestrator(&state_a, persona.clone(), model.clone(), provider).await;

        // Drive one turn so there's content to persist.
        let _ = read_body(
            router(state_a.clone())
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/conversation/turn")
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"speaker_id":"g","content":"hi"}"#))
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;

        let save_resp = router(state_a)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/conversation/save")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(save_resp.status(), StatusCode::OK);
        let saved: serde_json::Value = serde_json::from_str(&read_body(save_resp).await).unwrap();
        assert_eq!(saved["session_id"], "sess-test");

        // Second state: empty orchestrator, same store. Load should
        // hydrate it and snapshot should match what we saved.
        let state_b = build_state_with_store_dir(persona, model, tmp.path().to_path_buf());

        let load_resp = router(state_b.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/conversation/load")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "session_id": "sess-test",
                            "anthropic_key": "sk-test",
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(load_resp.status(), StatusCode::OK);

        let snap = router(state_b)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/conversation/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&read_body(snap).await).unwrap();
        assert_eq!(v["id"], "sess-test");
        assert_eq!(v["turns"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn save_without_active_session_is_409() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = build_state_with_store_dir(
            sample_persona("scholar", "m1", "x"),
            sample_model("m1"),
            tmp.path().to_path_buf(),
        );
        let resp = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/conversation/save")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn load_unknown_session_is_404() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = build_state_with_store_dir(
            sample_persona("scholar", "m1", "x"),
            sample_model("m1"),
            tmp.path().to_path_buf(),
        );
        let resp = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/conversation/load")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"session_id":"missing","anthropic_key":"k"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn load_with_invalid_id_is_400() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = build_state_with_store_dir(
            sample_persona("scholar", "m1", "x"),
            sample_model("m1"),
            tmp.path().to_path_buf(),
        );
        let resp = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/conversation/load")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"session_id":"../etc/passwd","anthropic_key":"k"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn load_without_configured_credential_is_412() {
        let tmp = tempfile::TempDir::new().unwrap();
        let persona = sample_persona("scholar", "m1", "x");
        let model = sample_model("m1");
        // Pre-seed a saved session on disk.
        let session = ConversationSession::new(
            "sess-x",
            Speaker::ai_agent("ai-scholar", "Scholar"),
            persona.id.clone(),
            model.id.clone(),
        );
        let store = FsSessionStore::new(tmp.path());
        store.save(&session).await.unwrap();

        // Empty SecretsManager: load must hit `provider_not_configured`.
        let registries = Arc::new(Registries {
            personas: [(persona.id.clone(), persona)].into(),
            models: [(model.id.clone(), model)].into(),
            prompts_dir: PathBuf::from("/nonexistent"),
            sessions_dir: tmp.path().to_path_buf(),
        });
        let store: Arc<dyn SessionStore> = Arc::new(FsSessionStore::new(tmp.path()));
        let state = ConversationApiState::with_store(
            registries,
            reqwest::Client::new(),
            store,
            empty_test_secrets(),
        );
        let resp = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/conversation/load")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"session_id":"sess-x"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test]
    async fn load_when_persona_no_longer_in_registry_is_422() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Save a session that uses persona 'gone'.
        let session = ConversationSession::new(
            "sess-orphan",
            Speaker::ai_agent("ai-gone", "Gone"),
            "gone".to_string(),
            "m1".to_string(),
        );
        FsSessionStore::new(tmp.path())
            .save(&session)
            .await
            .unwrap();

        // But the live registry only knows about 'scholar'.
        let state = build_state_with_store_dir(
            sample_persona("scholar", "m1", "x"),
            sample_model("m1"),
            tmp.path().to_path_buf(),
        );
        let resp = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/conversation/load")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"session_id":"sess-orphan","anthropic_key":"k"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn list_sessions_returns_sorted_ids() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = FsSessionStore::new(tmp.path());
        for id in ["beta", "alpha", "gamma"] {
            store
                .save(&ConversationSession::new(
                    id,
                    Speaker::ai_agent("ai-x", "X"),
                    "scholar".to_string(),
                    "m1".to_string(),
                ))
                .await
                .unwrap();
        }
        let state = build_state_with_store_dir(
            sample_persona("scholar", "m1", "x"),
            sample_model("m1"),
            tmp.path().to_path_buf(),
        );
        let resp = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/conversation/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&read_body(resp).await).unwrap();
        let sessions = v["sessions"].as_array().unwrap();
        let ids: Vec<&str> = sessions.iter().map(|s| s["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["alpha", "beta", "gamma"]);
        // No turns appended, so titles default to empty strings.
        for s in sessions {
            assert_eq!(s["title"].as_str().unwrap(), "");
        }
    }

    #[tokio::test]
    async fn delete_session_removes_file_and_404s_on_repeat() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = FsSessionStore::new(tmp.path());
        store
            .save(&ConversationSession::new(
                "doomed",
                Speaker::ai_agent("ai-x", "X"),
                "scholar".to_string(),
                "m1".to_string(),
            ))
            .await
            .unwrap();
        let state = build_state_with_store_dir(
            sample_persona("scholar", "m1", "x"),
            sample_model("m1"),
            tmp.path().to_path_buf(),
        );
        // First delete: 204.
        let resp = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/conversation/sessions/doomed")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        // Second delete: 404.
        let resp = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/conversation/sessions/doomed")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_session_rejects_invalid_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = build_state_with_store_dir(
            sample_persona("scholar", "m1", "x"),
            sample_model("m1"),
            tmp.path().to_path_buf(),
        );
        let resp = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    // Space (URL-encoded) is rejected by `validate_id`
                    // since it isn't in the allowed alphabet.
                    .uri("/conversation/sessions/has%20space")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn derive_title_uses_first_user_turn() {
        let mut s = ConversationSession::new(
            "t1",
            Speaker::ai_agent("ai-x", "X"),
            "scholar".to_string(),
            "m1".to_string(),
        );
        // AI turn first should be ignored; title pulls from the first
        // *user* turn, even when the AI replied earlier in the file.
        s.append_user_turn(
            "user".to_string(),
            "hello   world\nhow are you?".to_string(),
            0,
        );
        assert_eq!(derive_session_title(&s), "hello world how are you?");
    }

    #[test]
    fn derive_title_truncates_at_60_chars() {
        let mut s = ConversationSession::new(
            "t2",
            Speaker::ai_agent("ai-x", "X"),
            "scholar".to_string(),
            "m1".to_string(),
        );
        let long = "a".repeat(120);
        s.append_user_turn("user".to_string(), long, 0);
        let title = derive_session_title(&s);
        assert_eq!(title.chars().count(), 61); // 60 + ellipsis
        assert!(title.ends_with('\u{2026}'));
    }

    #[test]
    fn derive_title_empty_when_no_user_turns() {
        let s = ConversationSession::new(
            "t3",
            Speaker::ai_agent("ai-x", "X"),
            "scholar".to_string(),
            "m1".to_string(),
        );
        assert_eq!(derive_session_title(&s), "");
    }

    // ── TTS routes ─────────────────────────────────────────────

    /// Build a state with a TTS runtime rooted at `cache_root`. The
    /// runtime is shared with the caller so tests can pre-populate
    /// the cache or open broadcasts directly.
    fn build_state_with_tts_runtime(
        persona: Persona,
        model: ModelConfig,
        sessions_dir: PathBuf,
        cache_root: PathBuf,
    ) -> (ConversationApiState, TtsRuntime) {
        let registries = Arc::new(Registries {
            personas: [(persona.id.clone(), persona)].into(),
            models: [(model.id.clone(), model)].into(),
            prompts_dir: PathBuf::from("/nonexistent"),
            sessions_dir: sessions_dir.clone(),
        });
        let store: Arc<dyn SessionStore> = Arc::new(FsSessionStore::new(sessions_dir));
        let tts = TtsRuntime {
            hub: Arc::new(TtsHub::new()),
            cache: Arc::new(FsTtsCache::new(cache_root)),
        };
        let state = ConversationApiState::with_store_and_tts(
            registries,
            reqwest::Client::new(),
            store,
            test_secrets_with_default("sk-test"),
            tts.clone(),
        );
        (state, tts)
    }

    /// Pre-populate the cache file for `(session_id, turn_id)` with
    /// `bytes`. Drives [`FsTtsCache::writer`] just like the
    /// orchestrator does at runtime.
    async fn seed_cache(cache: &FsTtsCache, session_id: &str, turn_id: &str, bytes: &[u8]) {
        let mut w = cache.writer(session_id, turn_id).await.unwrap();
        w.write(bytes).await.unwrap();
        w.finish().await.unwrap();
    }

    #[tokio::test]
    async fn tts_routes_404_without_active_session_become_409() {
        // Without `init`, every conversation route returns 409 from
        // `require_session`. Both new TTS routes are no exception
        // \u2014 confirms they share the same gating contract.
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, _tts) = build_state_with_tts_runtime(
            sample_persona("scholar", "m1", "x"),
            sample_model("m1"),
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
        );
        for uri in [
            "/conversation/tts/turn-0001",
            "/conversation/tts/turn-0001/replay",
        ] {
            let resp = router(state.clone())
                .oneshot(
                    Request::builder()
                        .method(Method::GET)
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CONFLICT, "uri={uri}");
        }
    }

    #[tokio::test]
    async fn replay_returns_cached_mp3_with_audio_mpeg() {
        let tmp = tempfile::TempDir::new().unwrap();
        let persona = sample_persona("scholar", "m1", "x");
        let model = sample_model("m1");
        let (state, tts) = build_state_with_tts_runtime(
            persona.clone(),
            model.clone(),
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
        );
        let provider: Arc<dyn LlmProvider> =
            Arc::new(MockProvider::new("mock", vec![], TokenUsage::default()));
        install_orchestrator(&state, persona, model, provider).await;

        // Seed cache for the active session id (the test
        // orchestrator uses "sess-test").
        seed_cache(&tts.cache, "sess-test", "turn-0001", b"\xFF\xFB\x90payload").await;

        let resp = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/conversation/tts/turn-0001/replay")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "audio/mpeg"
        );
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(bytes.as_ref(), b"\xFF\xFB\x90payload");
    }

    #[tokio::test]
    async fn replay_returns_404_for_unknown_turn() {
        let tmp = tempfile::TempDir::new().unwrap();
        let persona = sample_persona("scholar", "m1", "x");
        let model = sample_model("m1");
        let (state, _tts) = build_state_with_tts_runtime(
            persona.clone(),
            model.clone(),
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
        );
        let provider: Arc<dyn LlmProvider> =
            Arc::new(MockProvider::new("mock", vec![], TokenUsage::default()));
        install_orchestrator(&state, persona, model, provider).await;

        let resp = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/conversation/tts/turn-9999/replay")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn stream_tts_returns_404_when_no_live_or_cache() {
        let tmp = tempfile::TempDir::new().unwrap();
        let persona = sample_persona("scholar", "m1", "x");
        let model = sample_model("m1");
        let (state, _tts) = build_state_with_tts_runtime(
            persona.clone(),
            model.clone(),
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
        );
        let provider: Arc<dyn LlmProvider> =
            Arc::new(MockProvider::new("mock", vec![], TokenUsage::default()));
        install_orchestrator(&state, persona, model, provider).await;

        let resp = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/conversation/tts/turn-9999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn stream_tts_emits_cache_only_when_no_live_broadcast() {
        let tmp = tempfile::TempDir::new().unwrap();
        let persona = sample_persona("scholar", "m1", "x");
        let model = sample_model("m1");
        let (state, tts) = build_state_with_tts_runtime(
            persona.clone(),
            model.clone(),
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
        );
        let provider: Arc<dyn LlmProvider> =
            Arc::new(MockProvider::new("mock", vec![], TokenUsage::default()));
        install_orchestrator(&state, persona, model, provider).await;
        seed_cache(&tts.cache, "sess-test", "turn-0001", b"\x01\x02\x03\x04").await;

        let resp = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/conversation/tts/turn-0001")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp).await;
        let frames = parse_sse(&body);
        let names: Vec<&str> = frames.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["audio", "done"]);

        let audio_payload: serde_json::Value = serde_json::from_str(&frames[0].1).unwrap();
        assert_eq!(audio_payload["type"], "audio");
        let decoded = BASE64_STANDARD
            .decode(audio_payload["b64"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, b"\x01\x02\x03\x04");
    }

    #[tokio::test]
    async fn stream_tts_late_subscriber_skips_frames_already_in_cache() {
        // Simulate the late-join handoff: a writer has already
        // pushed bytes to the cache, then publishes one more frame
        // whose `total_bytes_after` is *within* the snapshot we
        // read. The SSE handler must not re-emit the duplicate
        // bytes, but must emit the trailing frame whose offset
        // exceeds the snapshot.
        let tmp = tempfile::TempDir::new().unwrap();
        let persona = sample_persona("scholar", "m1", "x");
        let model = sample_model("m1");
        let (state, tts) = build_state_with_tts_runtime(
            persona.clone(),
            model.clone(),
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
        );
        let provider: Arc<dyn LlmProvider> =
            Arc::new(MockProvider::new("mock", vec![], TokenUsage::default()));
        install_orchestrator(&state, persona, model, provider).await;

        // Open a broadcast for turn-0001 and seed the cache with
        // 4 bytes (covers a hypothetical first frame whose
        // `total_bytes_after = 4`).
        let bcast = tts.hub.open("turn-0001".into());
        seed_cache(&tts.cache, "sess-test", "turn-0001", b"\x01\x02\x03\x04").await;

        // Spawn the SSE request. Drain on a separate task so we
        // can publish broadcasts after subscribe is in flight.
        let app = router(state);
        let handle = tokio::spawn(async move {
            let resp = app
                .oneshot(
                    Request::builder()
                        .method(Method::GET)
                        .uri("/conversation/tts/turn-0001")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            read_body(resp).await
        });

        // Give the handler a moment to subscribe + read cache.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Frame inside the cache window \u2014 must be dropped.
        bcast.send(TtsBroadcastFrame::Audio {
            bytes: vec![0x01, 0x02],
            total_bytes_after: 2,
        });
        // Frame past the cache window \u2014 must be emitted.
        bcast.send(TtsBroadcastFrame::Audio {
            bytes: vec![0x05, 0x06],
            total_bytes_after: 6,
        });
        bcast.finish();

        let body = handle.await.unwrap();
        let frames = parse_sse(&body);
        let names: Vec<&str> = frames.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["audio", "audio", "done"]);

        let snap_payload: serde_json::Value = serde_json::from_str(&frames[0].1).unwrap();
        let snap_bytes = BASE64_STANDARD
            .decode(snap_payload["b64"].as_str().unwrap())
            .unwrap();
        assert_eq!(snap_bytes, b"\x01\x02\x03\x04");

        let tail_payload: serde_json::Value = serde_json::from_str(&frames[1].1).unwrap();
        let tail_bytes = BASE64_STANDARD
            .decode(tail_payload["b64"].as_str().unwrap())
            .unwrap();
        assert_eq!(tail_bytes, vec![0x05, 0x06]);
    }

    #[tokio::test]
    async fn stream_tts_propagates_error_frame() {
        let tmp = tempfile::TempDir::new().unwrap();
        let persona = sample_persona("scholar", "m1", "x");
        let model = sample_model("m1");
        let (state, tts) = build_state_with_tts_runtime(
            persona.clone(),
            model.clone(),
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
        );
        let provider: Arc<dyn LlmProvider> =
            Arc::new(MockProvider::new("mock", vec![], TokenUsage::default()));
        install_orchestrator(&state, persona, model, provider).await;

        let bcast = tts.hub.open("turn-0001".into());
        let app = router(state);
        let handle = tokio::spawn(async move {
            let resp = app
                .oneshot(
                    Request::builder()
                        .method(Method::GET)
                        .uri("/conversation/tts/turn-0001")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            read_body(resp).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        bcast.fail("boom".into());

        let body = handle.await.unwrap();
        let frames = parse_sse(&body);
        let names: Vec<&str> = frames.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["error"]);
        let v: serde_json::Value = serde_json::from_str(&frames[0].1).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["message"], "boom");
    }
}
