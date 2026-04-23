//! Browser-side consumer for the proxy's `/conversation/*` HTTP API.
//!
//! This module owns a small Dioxus view that:
//!
//! 1. Lists the proxy's available personas and models on mount
//!    (`GET /personas`, `GET /models`).
//! 2. On the first user turn, calls `POST /conversation/init` with the
//!    selected persona + model + a generated session id. Anthropic
//!    credentials are resolved server-side from the proxy's keystore;
//!    the user picks which named credential to use via the dropdown
//!    (defaults to `default`).
//! 3. On every user turn, calls `POST /conversation/turn` and consumes
//!    the SSE response body chunk-by-chunk, accumulating
//!    `OrchestratorEvent::Token { delta }` into a streaming
//!    "in-progress" assistant bubble until `ai_turn_appended` arrives.
//!
//! The proxy returns SSE frames shaped as `event: <name>\ndata:
//! <json>\n\n`. The JSON payload itself carries the
//! `#[serde(tag = "type")]` discriminant, so parsing leans on the
//! `data:` line and ignores the `event:` line.
//!
//! There is no built-in Dioxus SSE client and no `EventSource` binding
//! we want to take a dependency on for a single use site, so the
//! reader hand-rolls a `ReadableStreamDefaultReader` loop with a
//! `TextDecoder` and splits frames on a blank line.

use std::cell::RefCell;
use std::rc::Rc;

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::{JsFuture, spawn_local};

const PROXY_BASE: &str = "http://127.0.0.1:3033";

// ── Wire payloads ────────────────────────────────────────────────────
//
// These mirror the proxy's payloads but are redeclared here to keep
// `parley` (WASM root) free of any dependency on the native-only proxy
// crate. `parley-core` is the shared types crate; we deliberately don't
// pull request/response shapes into it because they are HTTP contract
// surface, not domain.

// `session_initialized` keeps its outer `mut` because the bootstrap
// `use_future` doesn't touch it but the rsx (`disabled: ...`) reads it.
// `submit_turn` mutates its own destructured copy.

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // description is wire-only metadata; not yet rendered
struct PersonaSummary {
    id: String,
    name: String,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Clone, Deserialize)]
struct PersonaListResponse {
    personas: Vec<PersonaSummary>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // context_window is wire-only metadata; not yet rendered
struct ModelSummary {
    id: String,
    provider: String,
    model_name: String,
    #[serde(default)]
    context_window: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct ModelListResponse {
    models: Vec<ModelSummary>,
}

#[derive(Debug, Clone, Deserialize)]
struct SessionListResponse {
    sessions: Vec<SessionSummary>,
}

#[derive(Debug, Clone, Deserialize)]
struct SessionSummary {
    id: String,
    /// Best-effort title pulled from the first user turn server-side.
    /// Empty when the session has no user turns yet.
    #[serde(default)]
    title: String,
}

// `WireSession` mirrors `parley_core::ConversationSession` but only
// carries the fields the picker UI actually needs to rehydrate the
// view. `serde` ignores unknown fields by default, so adding more on
// the proxy side never breaks the frontend.
#[derive(Debug, Clone, Deserialize)]
struct WireSession {
    #[allow(dead_code)] // echoed by the proxy; we already know our own id
    id: String,
    turns: Vec<WireTurn>,
    persona_history: Vec<WirePersonaActivation>,
}

#[derive(Debug, Clone, Deserialize)]
struct WireTurn {
    /// `"user" | "assistant" | "system"` — drives the bubble color.
    role: String,
    content: String,
    /// Provenance is populated only for AI turns; user/system turns
    /// have no cost. We only need the cost out of it on the wire,
    /// so the nested struct intentionally ignores the rest.
    #[serde(default)]
    provenance: Option<WireProvenance>,
}

/// Subset of `parley_core::TurnProvenance` — only the cost fields
/// are rendered. `serde` ignores unknown fields.
#[derive(Debug, Clone, Deserialize)]
struct WireProvenance {
    #[serde(default)]
    llm_cost: WireCost,
    #[serde(default)]
    tts_cost: WireCost,
}

/// Mirrors `parley_core::Cost`.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
struct WireCost {
    #[serde(default)]
    usd: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct WirePersonaActivation {
    persona_id: String,
    model_config_id: String,
}

/// Mirrors `proxy::orchestrator::OrchestratorEvent`. The `#[serde(tag
/// = "type")]` discriminant lets us decode the union without knowing
/// the SSE event name up front.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)] // some fields are echoed by the proxy but not consumed yet
enum WireEvent {
    StateChanged {
        #[serde(default)]
        state: String,
    },
    UserTurnAppended {
        #[serde(default)]
        turn_id: String,
    },
    Token {
        delta: String,
    },
    AiTurnAppended {
        #[serde(default)]
        turn_id: String,
        /// Final USD cost for this turn. Carried as a nested struct
        /// to mirror `parley_core::Cost` on the wire.
        #[serde(default)]
        cost: WireCost,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug, Serialize)]
struct InitRequest<'a> {
    session_id: &'a str,
    persona_id: &'a str,
    ai_speaker_id: String,
    ai_speaker_label: &'a str,
    /// Named Anthropic credential to use. `None` means the proxy's
    /// `default` credential. The literal key never crosses the wire
    /// from the browser: the proxy resolves it from the keystore.
    #[serde(skip_serializing_if = "Option::is_none")]
    credential: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct TurnRequest<'a> {
    speaker_id: &'a str,
    content: &'a str,
}

// ── View state ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq)]
struct Message {
    role: Role,
    content: String,
    /// USD cost for this turn. `None` for user turns and for
    /// historical assistant turns whose session file lacks
    /// provenance (older sessions).
    cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
enum SendStatus {
    Idle,
    Sending,
    Streaming,
    Failed(String),
}

// ── HTTP helpers ─────────────────────────────────────────────────────

fn build_post(url: &str, body_json: &str) -> Result<web_sys::Request, String> {
    let opts = web_sys::RequestInit::new();
    opts.set_method("POST");
    opts.set_mode(web_sys::RequestMode::Cors);
    opts.set_body(&wasm_bindgen::JsValue::from_str(body_json));
    let headers = web_sys::Headers::new().map_err(|e| format!("{:?}", e))?;
    headers
        .set("Content-Type", "application/json")
        .map_err(|e| format!("{:?}", e))?;
    opts.set_headers(&headers);
    web_sys::Request::new_with_str_and_init(url, &opts).map_err(|e| format!("{:?}", e))
}

fn build_get(url: &str) -> Result<web_sys::Request, String> {
    let opts = web_sys::RequestInit::new();
    opts.set_method("GET");
    opts.set_mode(web_sys::RequestMode::Cors);
    web_sys::Request::new_with_str_and_init(url, &opts).map_err(|e| format!("{:?}", e))
}

fn build_delete(url: &str) -> Result<web_sys::Request, String> {
    let opts = web_sys::RequestInit::new();
    opts.set_method("DELETE");
    opts.set_mode(web_sys::RequestMode::Cors);
    web_sys::Request::new_with_str_and_init(url, &opts).map_err(|e| format!("{:?}", e))
}

async fn fetch_json<T>(req: web_sys::Request) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    let window = web_sys::window().ok_or("no window")?;
    let resp_val = JsFuture::from(window.fetch_with_request(&req))
        .await
        .map_err(|e| format!("network error: {:?}", e))?;
    let resp: web_sys::Response = resp_val
        .dyn_into()
        .map_err(|_| "fetch did not return a Response".to_string())?;
    let status = resp.status();
    let text_val = JsFuture::from(
        resp.text()
            .map_err(|e| format!("response.text(): {:?}", e))?,
    )
    .await
    .map_err(|e| format!("body read failed: {:?}", e))?;
    let text = text_val
        .as_string()
        .ok_or_else(|| "non-string response body".to_string())?;
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {text}"));
    }
    serde_json::from_str::<T>(&text).map_err(|e| format!("decode error: {e} (body: {text})"))
}

// ── SSE reader ───────────────────────────────────────────────────────

/// Read an SSE response chunk-by-chunk and invoke `on_event` for each
/// fully-decoded JSON payload. Returns when the stream ends or the
/// reader fails. Errors during streaming are surfaced as a synthetic
/// `WireEvent::Failed`.
async fn drain_sse<F>(resp: web_sys::Response, mut on_event: F)
where
    F: FnMut(WireEvent),
{
    let body = match resp.body() {
        Some(b) => b,
        None => {
            on_event(WireEvent::Failed {
                message: "response has no body".into(),
            });
            return;
        }
    };
    let reader_val = match body
        .get_reader()
        .dyn_into::<web_sys::ReadableStreamDefaultReader>()
    {
        Ok(r) => r,
        Err(_) => {
            on_event(WireEvent::Failed {
                message: "failed to acquire stream reader".into(),
            });
            return;
        }
    };
    let decoder = match web_sys::TextDecoder::new_with_label("utf-8") {
        Ok(d) => d,
        Err(_) => {
            on_event(WireEvent::Failed {
                message: "failed to construct TextDecoder".into(),
            });
            return;
        }
    };

    // SSE frames are separated by a blank line ("\n\n"). Bytes that
    // arrive split across reads accumulate in `buffer` until we see
    // the next boundary.
    let mut buffer = String::new();
    loop {
        let read_result = match JsFuture::from(reader_val.read()).await {
            Ok(v) => v,
            Err(e) => {
                on_event(WireEvent::Failed {
                    message: format!("stream read failed: {:?}", e),
                });
                return;
            }
        };
        let done = js_sys::Reflect::get(&read_result, &"done".into())
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let value = js_sys::Reflect::get(&read_result, &"value".into()).ok();

        if let Some(value) = value
            && !value.is_undefined()
            && !value.is_null()
        {
            // value is a Uint8Array; TextDecoder.decode wants a
            // BufferSource and accepts Uint8Array directly.
            let chunk = decoder
                .decode_with_buffer_source(&value.into())
                .unwrap_or_default();
            buffer.push_str(&chunk);
            // Emit every complete frame currently in the buffer.
            while let Some(idx) = buffer.find("\n\n") {
                let frame = buffer[..idx].to_string();
                buffer = buffer[idx + 2..].to_string();
                if let Some(json) = extract_data_payload(&frame)
                    && let Ok(ev) = serde_json::from_str::<WireEvent>(&json)
                {
                    on_event(ev);
                }
            }
        }

        if done {
            // Flush any trailing buffered frame (rare, but keepalive
            // comments and the like can leave bytes behind).
            let trailing = std::mem::take(&mut buffer);
            if let Some(json) = extract_data_payload(trailing.trim_end_matches('\n'))
                && let Ok(ev) = serde_json::from_str::<WireEvent>(&json)
            {
                on_event(ev);
            }
            return;
        }
    }
}

/// Pull the `data:` line out of an SSE frame. Comments (`:` prefix),
/// `event:` lines, and id/retry lines are ignored — the JSON payload
/// already carries the discriminant and we don't need stream ids.
fn extract_data_payload(frame: &str) -> Option<String> {
    let mut data: Option<String> = None;
    for line in frame.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            // Multi-line `data:` fields are allowed by the SSE spec
            // (concatenated with `\n`), but our server emits a single
            // `data:` per frame. Handle both anyway.
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            match data.as_mut() {
                Some(buf) => {
                    buf.push('\n');
                    buf.push_str(rest);
                }
                None => data = Some(rest.to_string()),
            }
        }
    }
    data
}

// ── Component ────────────────────────────────────────────────────────

/// Top-level Dioxus view that drives a single conversation against the
/// proxy.
#[component]
pub fn ConversationView() -> Element {
    // ── Local signals ────────────────────────────────────────────
    // Secrets status drives both the "is Anthropic configured?"
    // gate and the credential picker. Values themselves never reach
    // the browser — only configuration metadata.
    let (secrets_status, _) = crate::ui::secrets::use_secrets_status();
    let anthropic_credentials = use_memo(move || match &*secrets_status.read_unchecked() {
        Some(Ok(s)) => s.credential_names("anthropic"),
        _ => Vec::new(),
    });
    let mut selected_credential = use_signal(|| "default".to_string());
    let mut personas = use_signal(Vec::<PersonaSummary>::new);
    let mut models = use_signal(Vec::<ModelSummary>::new);
    let mut selected_persona = use_signal(String::new);
    let mut selected_model = use_signal(String::new);
    let mut session_id = use_signal(generate_session_id);
    let mut session_initialized = use_signal(|| false);
    let mut messages = use_signal(Vec::<Message>::new);
    let mut streaming = use_signal(String::new);
    let mut input = use_signal(String::new);
    let mut status = use_signal(|| SendStatus::Idle);
    let mut bootstrap_error: Signal<Option<String>> = use_signal(|| None);
    // Saved-session picker state. `available_sessions` is the dropdown
    // list (lexicographic from the proxy, which sorts the filenames).
    // `pick_session` is the user's current selection inside the
    // dropdown — distinct from `session_id` because the latter is the
    // id of the *active* session, not the one staged for loading.
    let mut available_sessions = use_signal(Vec::<SessionSummary>::new);
    let mut pick_session = use_signal(String::new);
    // `applied_*` mirror the persona/model that the *active* session
    // is currently bound to on the proxy. They diverge from
    // `selected_*` when the user changes a dropdown post-init; the
    // "Switch" button uses that delta to decide if it's actionable.
    let mut applied_persona = use_signal(String::new);
    let mut applied_model = use_signal(String::new);

    // ── Bootstrap: load personas + models on mount ───────────────
    use_future(move || async move {
        match build_get(&format!("{PROXY_BASE}/personas")) {
            Ok(req) => match fetch_json::<PersonaListResponse>(req).await {
                Ok(resp) => {
                    if let Some(first) = resp.personas.first() {
                        selected_persona.set(first.id.clone());
                    }
                    personas.set(resp.personas);
                }
                Err(e) => bootstrap_error.set(Some(format!("could not load personas: {e}"))),
            },
            Err(e) => bootstrap_error.set(Some(format!("could not build /personas request: {e}"))),
        }
        match build_get(&format!("{PROXY_BASE}/models")) {
            Ok(req) => match fetch_json::<ModelListResponse>(req).await {
                Ok(resp) => {
                    if let Some(first) = resp.models.first() {
                        selected_model.set(first.id.clone());
                    }
                    models.set(resp.models);
                }
                Err(e) => bootstrap_error.set(Some(format!("could not load models: {e}"))),
            },
            Err(e) => bootstrap_error.set(Some(format!("could not build /models request: {e}"))),
        }
        match build_get(&format!("{PROXY_BASE}/conversation/sessions")) {
            Ok(req) => match fetch_json::<SessionListResponse>(req).await {
                Ok(resp) => available_sessions.set(resp.sessions),
                Err(e) => bootstrap_error.set(Some(format!("could not load sessions: {e}"))),
            },
            Err(e) => bootstrap_error.set(Some(format!(
                "could not build /conversation/sessions request: {e}"
            ))),
        }
    });

    // ── Submit handler ────────────────────────────────────────────
    // Bundle every signal the submit pipeline touches into one
    // `Copy` struct so the same logic can be invoked from multiple
    // event handlers (button click + Ctrl+Enter) without sharing a
    // single FnMut closure between them.
    let handles = SendHandles {
        selected_credential,
        selected_persona,
        selected_model,
        session_id,
        session_initialized,
        messages,
        streaming,
        input,
        status,
        available_sessions,
        applied_persona,
        applied_model,
    };

    rsx! {
        // Scoped dark theme matching the transcription view's
        // palette (#1a1a2e bg, #16213e panel, #0f3460 highlight,
        // #e0e0e0 text, #4ecca3 success, #e94560 error). Kept inline
        // so this component stays self-contained — no global CSS
        // file changes needed for the slice.
        style {
            r#"
            .convo-root {{
                background: #1a1a2e;
                color: #e0e0e0;
                min-height: 100vh;
                box-sizing: border-box;
            }}
            .convo-root h2 {{ margin-top: 0; color: #e0e0e0; }}
            .convo-root label {{ color: #c0c0d0; font-size: 0.95rem; }}
            .convo-root input,
            .convo-root select,
            .convo-root textarea {{
                background: #16213e;
                color: #e0e0e0;
                border: 1px solid #2a3960;
                border-radius: 4px;
                padding: 0.4rem 0.6rem;
                font-family: inherit;
                font-size: 0.95rem;
            }}
            .convo-root input:focus,
            .convo-root select:focus,
            .convo-root textarea:focus {{
                outline: none;
                border-color: #4ecca3;
            }}
            .convo-root input:disabled,
            .convo-root select:disabled {{
                opacity: 0.55;
                cursor: not-allowed;
            }}
            .convo-root button.convo-send {{
                background: #0f3460;
                color: #e0e0e0;
                border: 1px solid #2a3960;
                border-radius: 4px;
                padding: 0.5rem 1rem;
                font-size: 1rem;
                cursor: pointer;
            }}
            .convo-root button.convo-send:hover:not(:disabled) {{
                background: #16213e;
                border-color: #4ecca3;
            }}
            .convo-root button.convo-send:disabled {{
                opacity: 0.5;
                cursor: not-allowed;
            }}
            .convo-root .convo-transcript {{
                background: #12284a;
                border: 1px solid #2a3960;
                border-radius: 6px;
                padding: 0.75rem;
                min-height: 240px;
                max-height: 60vh;
                overflow-y: auto;
                margin-bottom: 0.75rem;
            }}
            .convo-root .convo-msg-user {{
                background: #0f3460;
                border-left: 3px solid #4ecca3;
            }}
            .convo-root .convo-msg-ai {{
                background: #16213e;
                border-left: 3px solid #e9a545;
            }}
            .convo-root .convo-msg-streaming {{
                background: #16213e;
                border-left: 3px solid #e9a545;
                opacity: 0.85;
            }}
            .convo-root .convo-msg {{
                margin-bottom: 0.75rem;
                padding: 0.5rem 0.75rem;
                border-radius: 4px;
            }}
            .convo-root .convo-role {{
                font-size: 0.75rem;
                color: #8888aa;
                margin-bottom: 0.25rem;
                text-transform: uppercase;
                letter-spacing: 0.04em;
            }}
            .convo-root .convo-empty {{ color: #8888aa; font-style: italic; }}
            "#
        }
        div { class: "convo-root",
            style: "max-width: 760px; margin: 0 auto; padding: 1rem; font-family: system-ui, sans-serif;",

            h2 { "Conversation" }

            if let Some(err) = bootstrap_error() {
                div { style: "color: #ff6b81; padding: 0.5rem; border: 1px solid #e94560; border-radius: 4px; margin-bottom: 1rem; background: #3a1020;",
                    "Bootstrap error: {err}"
                }
            }

            // ── Saved-session picker ─────────────────────────
            // Lets the user resume a previously persisted session or
            // start a fresh one. The persona/model dropdowns lock to
            // whatever the loaded session was using — mid-session
            // switching of persona/model is intentionally deferred.
            div { style: "display: flex; gap: 0.5rem; align-items: center; margin-bottom: 0.75rem; flex-wrap: wrap;",
                label { r#for: "convo-pick", style: "color: #c0c0d0;", "Saved sessions" }
                select {
                    id: "convo-pick",
                    value: "{pick_session}",
                    onchange: move |e| pick_session.set(e.value()),
                    option { value: "", "— select a saved session —" }
                    for s in available_sessions().iter() {
                        option { key: "{s.id}", value: "{s.id}",
                            if s.title.is_empty() { "{s.id}" } else { "{s.title}" }
                        }
                    }
                }
                button {
                    class: "convo-send",
                    disabled: pick_session().is_empty()
                        || matches!(status(), SendStatus::Sending | SendStatus::Streaming),
                    onclick: move |_| {
                        let sid = pick_session.peek().clone();
                        if sid.is_empty() {
                            return;
                        }
                        let credential = selected_credential.peek().clone();
                        status.set(SendStatus::Sending);
                        spawn_local(async move {
                            let body = match serde_json::to_string(&serde_json::json!({
                                "session_id": sid,
                                "credential": credential,
                            })) {
                                Ok(s) => s,
                                Err(e) => {
                                    status.set(SendStatus::Failed(format!(
                                        "load payload: {e}"
                                    )));
                                    return;
                                }
                            };
                            let req = match build_post(
                                &format!("{PROXY_BASE}/conversation/load"),
                                &body,
                            ) {
                                Ok(r) => r,
                                Err(e) => {
                                    status.set(SendStatus::Failed(e));
                                    return;
                                }
                            };
                            match fetch_json::<WireSession>(req).await {
                                Ok(snap) => {
                                    // Replace transcript with loaded turns. System
                                    // turns (compaction summaries) are skipped —
                                    // they aren't user-visible content.
                                    let mut loaded = Vec::with_capacity(snap.turns.len());
                                    for t in snap.turns {
                                        let role = match t.role.as_str() {
                                            "user" => Some(Role::User),
                                            "assistant" => Some(Role::Assistant),
                                            _ => None,
                                        };
                                        if let Some(role) = role {
                                            let cost_usd = match role {
                                                Role::Assistant => {
                                                    // Sum LLM + TTS cost so the
                                                    // bubble shows the all-in
                                                    // figure for the turn.
                                                    t.provenance.as_ref().map(|p| {
                                                        p.llm_cost.usd + p.tts_cost.usd
                                                    })
                                                }
                                                Role::User => None,
                                            };
                                            loaded.push(Message {
                                                role,
                                                content: t.content,
                                                cost_usd,
                                            });
                                        }
                                    }
                                    messages.set(loaded);
                                    streaming.set(String::new());
                                    session_id.set(sid);
                                    // Surface the persona/model that the
                                    // loaded session is currently bound to.
                                    // Lock both dropdowns by flipping
                                    // session_initialized — mid-session
                                    // switching is a future slice.
                                    if let Some(active) = snap.persona_history.last() {
                                        selected_persona.set(active.persona_id.clone());
                                        selected_model.set(active.model_config_id.clone());
                                        applied_persona.set(active.persona_id.clone());
                                        applied_model.set(active.model_config_id.clone());
                                    }
                                    session_initialized.set(true);
                                    status.set(SendStatus::Idle);
                                }
                                Err(e) => {
                                    status.set(SendStatus::Failed(format!(
                                        "load failed: {e}"
                                    )));
                                }
                            }
                        });
                    },
                    "Load"
                }
                button {
                    class: "convo-send",
                    disabled: matches!(status(), SendStatus::Sending | SendStatus::Streaming),
                    onclick: move |_| {
                        // Reset to a fresh session. Persona/model
                        // dropdowns re-enable so the user can pick
                        // again before the next turn.
                        messages.set(Vec::new());
                        streaming.set(String::new());
                        input.set(String::new());
                        session_id.set(generate_session_id());
                        session_initialized.set(false);
                        pick_session.set(String::new());
                        applied_persona.set(String::new());
                        applied_model.set(String::new());
                        status.set(SendStatus::Idle);
                    },
                    "New"
                }
                button {
                    class: "convo-send",
                    disabled: pick_session().is_empty()
                        || matches!(status(), SendStatus::Sending | SendStatus::Streaming),
                    onclick: move |_| {
                        // Delete the saved session currently selected
                        // in the picker. Does NOT touch the active
                        // session — if the user just deleted the file
                        // for an in-progress session, the next turn's
                        // auto-save will recreate it.
                        let target = pick_session.peek().clone();
                        if target.is_empty() {
                            return;
                        }
                        spawn_local(async move {
                            let url = format!(
                                "{PROXY_BASE}/conversation/sessions/{target}"
                            );
                            let req = match build_delete(&url) {
                                Ok(r) => r,
                                Err(e) => {
                                    status.set(SendStatus::Failed(e));
                                    return;
                                }
                            };
                            let window = match web_sys::window() {
                                Some(w) => w,
                                None => return,
                            };
                            match JsFuture::from(window.fetch_with_request(&req)).await {
                                Ok(resp_val) => {
                                    if let Ok(resp) =
                                        resp_val.dyn_into::<web_sys::Response>()
                                        && !resp.ok()
                                    {
                                        status.set(SendStatus::Failed(format!(
                                            "delete failed: HTTP {}",
                                            resp.status()
                                        )));
                                        return;
                                    }
                                }
                                Err(e) => {
                                    status.set(SendStatus::Failed(format!(
                                        "delete request failed: {:?}",
                                        e
                                    )));
                                    return;
                                }
                            }
                            // Refresh the picker list and clear the
                            // selection. Failure is silent — picker
                            // state is non-critical.
                            if let Ok(req) = build_get(&format!(
                                "{PROXY_BASE}/conversation/sessions"
                            )) && let Ok(resp) =
                                fetch_json::<SessionListResponse>(req).await
                            {
                                available_sessions.set(resp.sessions);
                            }
                            pick_session.set(String::new());
                        });
                    },
                    "Delete"
                }
            }

            // ── Settings row ──────────────────────────────────
            div { style: "display: grid; grid-template-columns: auto 1fr; gap: 0.5rem 0.75rem; align-items: center; margin-bottom: 0.75rem;",
                label { r#for: "convo-credential", "Anthropic credential" }
                select {
                    id: "convo-credential",
                    value: "{selected_credential}",
                    disabled: matches!(status(), SendStatus::Sending | SendStatus::Streaming)
                        || anthropic_credentials().is_empty(),
                    onchange: move |e| selected_credential.set(e.value()),
                    if anthropic_credentials().is_empty() {
                        option { value: "default", "(no credentials configured)" }
                    } else {
                        for name in anthropic_credentials().iter() {
                            option { key: "{name}", value: "{name}", "{name}" }
                        }
                    }
                }

                label { r#for: "convo-persona", "Persona" }
                select {
                    id: "convo-persona",
                    value: "{selected_persona}",
                    disabled: matches!(status(), SendStatus::Sending | SendStatus::Streaming),
                    onchange: move |e| selected_persona.set(e.value()),
                    for p in personas().iter() {
                        option { key: "{p.id}", value: "{p.id}",
                            "{p.name} ({p.id})"
                        }
                    }
                }

                label { r#for: "convo-model", "Model" }
                select {
                    id: "convo-model",
                    value: "{selected_model}",
                    disabled: matches!(status(), SendStatus::Sending | SendStatus::Streaming),
                    onchange: move |e| selected_model.set(e.value()),
                    for m in models().iter() {
                        option { key: "{m.id}", value: "{m.id}",
                            "{m.id} ({m.provider} / {m.model_name})"
                        }
                    }
                }

                label { r#for: "convo-session", "Session id" }
                input {
                    id: "convo-session",
                    r#type: "text",
                    value: "{session_id}",
                    disabled: session_initialized(),
                    oninput: move |e| session_id.set(e.value()),
                }

                // Switch button: rebinds the active session to the
                // currently-selected persona/model. Only meaningful
                // post-init and when the dropdowns have actually
                // drifted from what the proxy currently has bound.
                button {
                    class: "convo-send",
                    disabled: !session_initialized()
                        || matches!(status(), SendStatus::Sending | SendStatus::Streaming)
                        || (selected_persona() == applied_persona()
                            && selected_model() == applied_model()),
                    onclick: move |_| {
                        let new_persona = selected_persona.peek().clone();
                        let new_model = selected_model.peek().clone();
                        let credential = selected_credential.peek().clone();
                        status.set(SendStatus::Sending);
                        spawn_local(async move {
                            let body = match serde_json::to_string(&serde_json::json!({
                                "persona_id": new_persona,
                                "model_config_id": new_model,
                                "credential": credential,
                            })) {
                                Ok(s) => s,
                                Err(e) => {
                                    status.set(SendStatus::Failed(format!("switch payload: {e}")));
                                    return;
                                }
                            };
                            let req = match build_post(
                                &format!("{PROXY_BASE}/conversation/switch"),
                                &body,
                            ) {
                                Ok(r) => r,
                                Err(e) => {
                                    status.set(SendStatus::Failed(e));
                                    return;
                                }
                            };
                            // /switch returns 204 No Content on success;
                            // we just need the HTTP status, not a body.
                            let window = match web_sys::window() {
                                Some(w) => w,
                                None => return,
                            };
                            match JsFuture::from(window.fetch_with_request(&req)).await {
                                Ok(resp_val) => {
                                    let resp = match resp_val.dyn_into::<web_sys::Response>() {
                                        Ok(r) => r,
                                        Err(_) => {
                                            status.set(SendStatus::Failed(
                                                "switch: bad response".into(),
                                            ));
                                            return;
                                        }
                                    };
                                    if !resp.ok() {
                                        status.set(SendStatus::Failed(format!(
                                            "switch failed: HTTP {}",
                                            resp.status()
                                        )));
                                        return;
                                    }
                                    applied_persona.set(new_persona);
                                    applied_model.set(new_model);
                                    status.set(SendStatus::Idle);
                                }
                                Err(e) => {
                                    status.set(SendStatus::Failed(format!(
                                        "switch request failed: {:?}",
                                        e
                                    )));
                                }
                            }
                        });
                    },
                    "Switch"
                }
            }

            // ── Transcript ────────────────────────────────────
            div { class: "convo-transcript",
                if messages().is_empty() && streaming().is_empty() {
                    div { class: "convo-empty",
                        "No messages yet. Type something below to start."
                    }
                }
                for (i, msg) in messages().iter().enumerate() {
                    div { key: "msg-{i}",
                        class: match msg.role {
                            Role::User => "convo-msg convo-msg-user",
                            Role::Assistant => "convo-msg convo-msg-ai",
                        },
                        div { class: "convo-role",
                            match msg.role { Role::User => "You", Role::Assistant => "Assistant" }
                        }
                        div { style: "white-space: pre-wrap;", "{msg.content}" }
                        if let Some(usd) = msg.cost_usd {
                            div {
                                style: "margin-top: 0.25rem; font-size: 0.75rem; color: #8888aa;",
                                "${usd:.4}"
                            }
                        }
                    }
                }
                if !streaming().is_empty() {
                    div { key: "{\"streaming\"}",
                        class: "convo-msg convo-msg-streaming",
                        div { class: "convo-role", "Assistant (typing…)" }
                        div { style: "white-space: pre-wrap;", "{streaming}" }
                    }
                }
            }

            // ── Status row ───────────────────────────────────
            // Running total includes every assistant turn the
            // current view has surfaced — including loaded
            // historical turns whose session file carried
            // provenance.
            {
                let total_usd: f64 = messages().iter().filter_map(|m| m.cost_usd).sum();
                rsx! {
                    div { style: "min-height: 1.25rem; margin-bottom: 0.5rem; font-size: 0.875rem; display: flex; gap: 1rem; align-items: center;",
                        match status() {
                            SendStatus::Idle => rsx! { span { style: "color: #8888aa;", "Ready" } },
                            SendStatus::Sending => rsx! { span { style: "color: #c0c0d0;", "Sending…" } },
                            SendStatus::Streaming => rsx! { span { style: "color: #4ecca3;", "Streaming…" } },
                            SendStatus::Failed(msg) => rsx! {
                                span { style: "color: #ff6b81;", "Error: {msg}" }
                                button {
                                    class: "convo-send",
                                    style: "padding: 0.25rem 0.5rem; font-size: 0.8rem;",
                                    onclick: move |_| retry_pending(handles),
                                    "Retry"
                                }
                                button {
                                    class: "convo-send",
                                    style: "padding: 0.25rem 0.5rem; font-size: 0.8rem;",
                                    onclick: move |_| dismiss_pending(handles),
                                    "Dismiss"
                                }
                            },
                        }
                        if total_usd > 0.0 {
                            span { style: "color: #8888aa; margin-left: auto;",
                                "Total: ${total_usd:.4}"
                            }
                        }
                    }
                }
            }

            // ── Composer ─────────────────────────────────────
            div { style: "display: flex; gap: 0.5rem;",
                textarea {
                    style: "flex: 1; min-height: 4rem; resize: vertical;",
                    placeholder: "Type a message and press Ctrl+Enter to send.",
                    value: "{input}",
                    oninput: move |e| input.set(e.value()),
                    onkeydown: move |e| {
                        let key = e.key();
                        let is_enter = matches!(key, Key::Enter);
                        if is_enter && (e.modifiers().ctrl() || e.modifiers().meta()) {
                            e.prevent_default();
                            submit_turn(handles);
                        }
                    },
                }
                button {
                    class: "convo-send",
                    disabled: matches!(status(), SendStatus::Sending | SendStatus::Streaming),
                    onclick: move |_| submit_turn(handles),
                    "Send"
                }
            }
        }
    }
}

/// Bundle of every signal `submit_turn` mutates. Signals in Dioxus
/// 0.7 are `Copy`, so this struct is `Copy` too — letting us hand the
/// same set of handles to multiple event handlers without per-handler
/// closures fighting over a single FnMut.
#[derive(Copy, Clone)]
struct SendHandles {
    selected_credential: Signal<String>,
    selected_persona: Signal<String>,
    selected_model: Signal<String>,
    session_id: Signal<String>,
    session_initialized: Signal<bool>,
    messages: Signal<Vec<Message>>,
    streaming: Signal<String>,
    input: Signal<String>,
    status: Signal<SendStatus>,
    /// Refreshed after every successful auto-save so a freshly
    /// created session id appears in the picker without a manual
    /// reload.
    available_sessions: Signal<Vec<SessionSummary>>,
    /// Persona/model the active session was last bound to on the
    /// proxy. Updated on init/load/switch success; used to detect
    /// when the dropdowns have drifted from the active state.
    applied_persona: Signal<String>,
    applied_model: Signal<String>,
}

/// Validate inputs, optimistically render the user turn, kick off the
/// async POST to the proxy, and stream the response into `streaming`
/// and `messages`. Reads/writes flow exclusively through the bundled
/// `Signal`s, so this function is callable from any event handler
/// without lifetime gymnastics.
fn submit_turn(h: SendHandles) {
    // Snapshot all inputs synchronously. The async task runs outside
    // the render scope and must not call `read()` (would panic) — we
    // use `peek()` here and then move owned `String`s into the future.
    let SendHandles {
        selected_credential,
        selected_persona,
        selected_model,
        session_id,
        mut session_initialized,
        mut messages,
        mut streaming,
        mut input,
        mut status,
        available_sessions,
        mut applied_persona,
        mut applied_model,
    } = h;

    let user_text = input.peek().trim().to_string();
    if user_text.is_empty() {
        return;
    }
    let credential = selected_credential.peek().clone();
    let persona = selected_persona.peek().clone();
    let model = selected_model.peek().clone();
    let sid = session_id.peek().clone();
    let already_init = *session_initialized.peek();

    if persona.is_empty() {
        status.set(SendStatus::Failed("select a persona first".into()));
        return;
    }

    // Optimistically render the user turn and clear the composer.
    messages.with_mut(|m| {
        m.push(Message {
            role: Role::User,
            content: user_text.clone(),
            cost_usd: None,
        })
    });
    input.set(String::new());
    streaming.set(String::new());
    status.set(SendStatus::Sending);

    spawn_local(async move {
        // Lazily init on first send.
        if !already_init {
            let init_body = match serde_json::to_string(&InitRequest {
                session_id: &sid,
                persona_id: &persona,
                ai_speaker_id: format!("ai-{persona}"),
                ai_speaker_label: &persona,
                credential: Some(credential.as_str()),
            }) {
                Ok(s) => s,
                Err(e) => {
                    status.set(SendStatus::Failed(format!("init payload: {e}")));
                    return;
                }
            };
            let req = match build_post(&format!("{PROXY_BASE}/conversation/init"), &init_body) {
                Ok(r) => r,
                Err(e) => {
                    status.set(SendStatus::Failed(e));
                    return;
                }
            };
            if let Err(e) = fetch_json::<serde_json::Value>(req).await {
                status.set(SendStatus::Failed(format!("init failed: {e}")));
                return;
            }
            session_initialized.set(true);
            applied_persona.set(persona.clone());
            applied_model.set(model.clone());
        }

        // Submit the turn.
        let turn_body = match serde_json::to_string(&TurnRequest {
            speaker_id: "user",
            content: &user_text,
        }) {
            Ok(s) => s,
            Err(e) => {
                status.set(SendStatus::Failed(format!("turn payload: {e}")));
                return;
            }
        };
        let req = match build_post(&format!("{PROXY_BASE}/conversation/turn"), &turn_body) {
            Ok(r) => r,
            Err(e) => {
                status.set(SendStatus::Failed(e));
                return;
            }
        };
        let window = match web_sys::window() {
            Some(w) => w,
            None => {
                status.set(SendStatus::Failed("no window".into()));
                return;
            }
        };
        let resp_val = match JsFuture::from(window.fetch_with_request(&req)).await {
            Ok(v) => v,
            Err(e) => {
                status.set(SendStatus::Failed(format!("turn request failed: {:?}", e)));
                return;
            }
        };
        let resp: web_sys::Response = match resp_val.dyn_into() {
            Ok(r) => r,
            Err(_) => {
                status.set(SendStatus::Failed("turn: not a Response".into()));
                return;
            }
        };
        if !resp.ok() {
            let body = match resp.text() {
                Ok(p) => JsFuture::from(p)
                    .await
                    .ok()
                    .and_then(|v| v.as_string())
                    .unwrap_or_default(),
                Err(_) => String::new(),
            };
            status.set(SendStatus::Failed(format!(
                "HTTP {}: {body}",
                resp.status()
            )));
            return;
        }

        status.set(SendStatus::Streaming);
        consume_turn_response(resp, messages, streaming, status, available_sessions).await;
    });
}

/// Re-dispatch the session's pending tail user turn after a failure.
/// POSTs `/conversation/retry` (no body — the proxy reads from the
/// session) and consumes the SSE stream the same way `submit_turn`
/// does. Leaves the optimistically-rendered user message in place.
fn retry_pending(h: SendHandles) {
    let SendHandles {
        messages,
        streaming,
        mut status,
        available_sessions,
        ..
    } = h;
    status.set(SendStatus::Sending);
    spawn_local(async move {
        let req = match build_post(&format!("{PROXY_BASE}/conversation/retry"), "") {
            Ok(r) => r,
            Err(e) => {
                status.set(SendStatus::Failed(e));
                return;
            }
        };
        let window = match web_sys::window() {
            Some(w) => w,
            None => {
                status.set(SendStatus::Failed("no window".into()));
                return;
            }
        };
        let resp_val = match JsFuture::from(window.fetch_with_request(&req)).await {
            Ok(v) => v,
            Err(e) => {
                status.set(SendStatus::Failed(format!("retry request failed: {:?}", e)));
                return;
            }
        };
        let resp: web_sys::Response = match resp_val.dyn_into() {
            Ok(r) => r,
            Err(_) => {
                status.set(SendStatus::Failed("retry: not a Response".into()));
                return;
            }
        };
        if !resp.ok() {
            status.set(SendStatus::Failed(format!(
                "retry failed: HTTP {}",
                resp.status()
            )));
            return;
        }
        status.set(SendStatus::Streaming);
        consume_turn_response(resp, messages, streaming, status, available_sessions).await;
    });
}

/// Discard the session's pending tail user turn after a failure.
/// POSTs `/conversation/discard_pending`, then pops the trailing
/// user `Message` from the local view so the UI matches the proxy
/// state. Clears `Failed` to `Idle` regardless of outcome (a
/// failed discard is logged as the new status).
fn dismiss_pending(h: SendHandles) {
    let SendHandles {
        mut messages,
        mut status,
        ..
    } = h;
    spawn_local(async move {
        let req = match build_post(&format!("{PROXY_BASE}/conversation/discard_pending"), "") {
            Ok(r) => r,
            Err(e) => {
                status.set(SendStatus::Failed(e));
                return;
            }
        };
        let window = match web_sys::window() {
            Some(w) => w,
            None => return,
        };
        match JsFuture::from(window.fetch_with_request(&req)).await {
            Ok(resp_val) => {
                if let Ok(resp) = resp_val.dyn_into::<web_sys::Response>()
                    && !resp.ok()
                {
                    status.set(SendStatus::Failed(format!(
                        "dismiss failed: HTTP {}",
                        resp.status()
                    )));
                    return;
                }
            }
            Err(e) => {
                status.set(SendStatus::Failed(format!(
                    "dismiss request failed: {:?}",
                    e
                )));
                return;
            }
        }
        // Pop the orphan user message from the local view so the UI
        // matches the now-clean session state.
        messages.with_mut(|m| {
            if matches!(m.last(), Some(msg) if msg.role == Role::User) {
                m.pop();
            }
        });
        status.set(SendStatus::Idle);
    });
}

/// Drain an SSE response body from `/conversation/turn` (or
/// `/conversation/retry`) into the message log. Owns the post-stream
/// status update + auto-save + picker refresh. Extracted so both
/// fresh submissions and retries share the exact same consumer
/// behavior.
async fn consume_turn_response(
    resp: web_sys::Response,
    mut messages: Signal<Vec<Message>>,
    mut streaming: Signal<String>,
    mut status: Signal<SendStatus>,
    mut available_sessions: Signal<Vec<SessionSummary>>,
) {
    // `drain_sse` takes an `FnMut`, so the closure can capture
    // signal handles by value (they're Copy) and mutate them
    // through `with_mut` / `set`. Failures during the stream are
    // routed through a shared cell and surfaced after the loop
    // ends, so we don't dispatch a status update from within the
    // event callback (which would interleave with token writes).
    // Auto-save signaling rides on the same channel: when the
    // assistant turn finalizes we set `saved_pending = true` and
    // fire the `/conversation/save` POST after the stream ends.
    let failure: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let save_pending: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
    let failure_inner = failure.clone();
    let save_inner = save_pending.clone();
    drain_sse(resp, move |ev| match ev {
        WireEvent::Token { delta } => {
            streaming.with_mut(|s| s.push_str(&delta));
        }
        WireEvent::AiTurnAppended { cost, .. } => {
            let final_text = streaming.peek().clone();
            streaming.set(String::new());
            messages.with_mut(|m| {
                m.push(Message {
                    role: Role::Assistant,
                    content: final_text,
                    cost_usd: Some(cost.usd),
                })
            });
            *save_inner.borrow_mut() = true;
        }
        WireEvent::Failed { message } => {
            *failure_inner.borrow_mut() = Some(message);
        }
        WireEvent::StateChanged { .. } | WireEvent::UserTurnAppended { .. } => {}
    })
    .await;

    if let Some(msg) = failure.borrow().as_ref() {
        status.set(SendStatus::Failed(msg.clone()));
    } else {
        status.set(SendStatus::Idle);
    }

    // Auto-save after the assistant turn lands. Done after the
    // status update so a save failure can override `Idle` with a
    // diagnostic without racing the streaming UI. We don't auto-
    // save on `Failed` because the proxy's session state will not
    // include the failed exchange anyway.
    if *save_pending.borrow() {
        match build_post(&format!("{PROXY_BASE}/conversation/save"), "") {
            Ok(req) => {
                if let Err(e) = fetch_json::<serde_json::Value>(req).await {
                    // Surface as a soft warning via SendStatus::Failed —
                    // the conversation itself is fine, only persistence
                    // failed. The user can retry on their next turn.
                    status.set(SendStatus::Failed(format!("auto-save failed: {e}")));
                } else if let Ok(req) = build_get(&format!("{PROXY_BASE}/conversation/sessions"))
                    && let Ok(resp) = fetch_json::<SessionListResponse>(req).await
                {
                    // Refresh the picker so a brand-new session id
                    // is selectable on the very next turn. Failure
                    // here is silent — the picker is non-critical.
                    available_sessions.set(resp.sessions);
                }
            }
            Err(e) => {
                status.set(SendStatus::Failed(format!("auto-save build failed: {e}")));
            }
        }
    }
}

/// Generate a short, file-safe session id. The proxy's
/// `FsSessionStore` enforces an ASCII-allowlist (alphanumerics, `_`,
/// `-`, `.`); we stick to that and prefix with the current epoch
/// seconds so the ids sort chronologically when listed.
fn generate_session_id() -> String {
    // `js_sys::Date::now()` returns ms since the epoch as f64. We
    // truncate to a u64 so the id sorts chronologically when listed.
    let now = js_sys::Date::now() as u64;
    let rand = (js_sys::Math::random() * 1_000_000.0) as u64;
    format!("sess-{now}-{rand}")
}
