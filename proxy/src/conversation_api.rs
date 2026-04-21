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
//!
//! ## Scope of this slice
//!
//! - Single in-process session. Re-issuing `init` replaces the
//!   previous session. The on-disk session file format is a separate
//!   slice.
//! - Provider construction supports Anthropic only; other
//!   `LlmProviderTag` values return `501 Not Implemented`. The
//!   `anthropic_key` is kept in memory inside the constructed
//!   `AnthropicLlm`; it is never logged or echoed.
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
    extract::State,
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
};
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

/// Immutable view of the registries loaded once at proxy boot.
/// Cheap to share via `Arc`.
pub struct Registries {
    /// All loaded personas, keyed by id.
    pub personas: HashMap<PersonaId, Persona>,
    /// All loaded model configs, keyed by id.
    pub models: HashMap<ModelConfigId, ModelConfig>,
    /// Directory holding `SystemPrompt::File` referents.
    pub prompts_dir: PathBuf,
}

/// Shared state behind the conversation routes.
///
/// `inner` holds the live orchestrator (None until first `init`).
/// `registries` is the immutable view of personas/models loaded at
/// boot. `http` is the shared reqwest client used to construct
/// real providers.
#[derive(Clone)]
pub struct ConversationApiState {
    inner: Arc<Mutex<Option<Arc<ConversationOrchestrator>>>>,
    registries: Arc<Registries>,
    http: reqwest::Client,
}

impl ConversationApiState {
    /// Build a new state wrapper around the supplied registries and
    /// HTTP client.
    pub fn new(registries: Arc<Registries>, http: reqwest::Client) -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
            registries,
            http,
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
        .route("/conversation/switch", post(switch_persona))
        .route("/conversation/snapshot", get(session_snapshot))
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
    /// Anthropic API key, used only when the active model's provider
    /// tag is `Anthropic`. Kept in memory inside the constructed
    /// `AnthropicLlm`. Never logged.
    #[serde(default)]
    pub anthropic_key: Option<String>,
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
    let provider = build_provider(&model, &req, &state.http)?;

    let session = ConversationSession::new(
        req.session_id,
        Speaker::ai_agent(req.ai_speaker_id, &req.ai_speaker_label),
        persona.id.clone(),
        model.id.clone(),
    );
    let snapshot = session.clone();
    let ctx = OrchestratorContext {
        personas: state.registries.personas.clone(),
        models: state.registries.models.clone(),
        providers: HashMap::from([(model.id.clone(), provider)]),
        prompts_dir: state.registries.prompts_dir.clone(),
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
    if !state.registries.models.contains_key(&req.model_config_id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorBody::new(format!(
                "unknown model '{}'",
                req.model_config_id
            ))),
        ));
    }
    orchestrator
        .switch_persona(req.persona_id, req.model_config_id)
        .await;
    Ok(StatusCode::NO_CONTENT)
}

async fn session_snapshot(
    State(state): State<ConversationApiState>,
) -> Result<Json<ConversationSession>, (StatusCode, Json<ErrorBody>)> {
    let orchestrator = require_session(&state).await?;
    Ok(Json(orchestrator.session_snapshot().await))
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
    req: &InitRequest,
    http: &reqwest::Client,
) -> Result<Arc<dyn LlmProvider>, (StatusCode, Json<ErrorBody>)> {
    match model.provider {
        LlmProviderTag::Anthropic => {
            let key = req.anthropic_key.clone().ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorBody::new(
                        "model uses Anthropic but no `anthropic_key` was supplied",
                    )),
                )
            })?;
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
    };
    (status, Json(ErrorBody::new(err.to_string())))
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
        OrchestratorEvent::Failed { .. } => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::test_support::{MockItem, MockProvider};
    use crate::orchestrator::{ConversationOrchestrator, OrchestratorContext};
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
        };
        let orch = Arc::new(ConversationOrchestrator::new(session, ctx));
        state.install_for_test(orch.clone()).await;
        orch
    }

    fn build_state(persona: Persona, model: ModelConfig) -> ConversationApiState {
        ConversationApiState::new(registries_with(persona, model), reqwest::Client::new())
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
        });
        let state = ConversationApiState::new(registries, reqwest::Client::new());
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
                        r#"{"persona_id":"ghost","model_config_id":"m1"}"#,
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
    async fn init_anthropic_without_key_is_400() {
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
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
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
}
