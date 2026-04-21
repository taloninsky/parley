//! Browser-side consumer for the proxy's `/conversation/*` HTTP API.
//!
//! This module owns a small Dioxus view that:
//!
//! 1. Lists the proxy's available personas and models on mount
//!    (`GET /personas`, `GET /models`).
//! 2. On the first user turn, calls `POST /conversation/init` with the
//!    selected persona + model + a generated session id and the
//!    `parley_anthropic_key` cookie.
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
    anthropic_key: Option<String>,
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
}

#[derive(Debug, Clone, PartialEq)]
enum SendStatus {
    Idle,
    Sending,
    Streaming,
    Failed(String),
}

// ── Cookie helpers (mirrors `app::load`/`app::save`) ────────────────

fn read_cookie(key: &str) -> Option<String> {
    let doc = web_sys::window()?.document()?;
    let cookies = js_sys::Reflect::get(&doc, &"cookie".into())
        .ok()?
        .as_string()?;
    for pair in cookies.split(';') {
        let pair = pair.trim();
        if let Some((k, v)) = pair.split_once('=')
            && k == key
        {
            return Some(v.to_string());
        }
    }
    None
}

fn write_cookie(key: &str, value: &str) {
    if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
        let cookie = format!(
            "{}={}; path=/; max-age=315360000; SameSite=Strict",
            key, value
        );
        let _ = js_sys::Reflect::set(&doc, &"cookie".into(), &cookie.into());
    }
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
    let mut anthropic_key = use_signal(|| read_cookie("parley_anthropic_key").unwrap_or_default());
    let mut personas = use_signal(Vec::<PersonaSummary>::new);
    let mut models = use_signal(Vec::<ModelSummary>::new);
    let mut selected_persona = use_signal(String::new);
    let mut selected_model = use_signal(String::new);
    let mut session_id = use_signal(generate_session_id);
    let session_initialized = use_signal(|| false);
    let messages = use_signal(Vec::<Message>::new);
    let streaming = use_signal(String::new);
    let mut input = use_signal(String::new);
    let status = use_signal(|| SendStatus::Idle);
    let mut bootstrap_error: Signal<Option<String>> = use_signal(|| None);

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
    });

    // ── Submit handler ────────────────────────────────────────────
    // Bundle every signal the submit pipeline touches into one
    // `Copy` struct so the same logic can be invoked from multiple
    // event handlers (button click + Ctrl+Enter) without sharing a
    // single FnMut closure between them.
    let handles = SendHandles {
        anthropic_key,
        selected_persona,
        selected_model,
        session_id,
        session_initialized,
        messages,
        streaming,
        input,
        status,
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

            // ── Settings row ──────────────────────────────────
            div { style: "display: grid; grid-template-columns: auto 1fr; gap: 0.5rem 0.75rem; align-items: center; margin-bottom: 0.75rem;",
                label { r#for: "convo-key", "Anthropic key" }
                input {
                    id: "convo-key",
                    r#type: "password",
                    value: "{anthropic_key}",
                    placeholder: "sk-ant-...",
                    oninput: move |e| {
                        let v = e.value();
                        write_cookie("parley_anthropic_key", &v);
                        anthropic_key.set(v);
                    },
                }

                label { r#for: "convo-persona", "Persona" }
                select {
                    id: "convo-persona",
                    value: "{selected_persona}",
                    disabled: session_initialized(),
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
                    disabled: session_initialized(),
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
            div { style: "min-height: 1.25rem; margin-bottom: 0.5rem; font-size: 0.875rem;",
                match status() {
                    SendStatus::Idle => rsx! { span { style: "color: #8888aa;", "Ready" } },
                    SendStatus::Sending => rsx! { span { style: "color: #c0c0d0;", "Sending…" } },
                    SendStatus::Streaming => rsx! { span { style: "color: #4ecca3;", "Streaming…" } },
                    SendStatus::Failed(msg) => rsx! { span { style: "color: #ff6b81;", "Error: {msg}" } },
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
    anthropic_key: Signal<String>,
    selected_persona: Signal<String>,
    selected_model: Signal<String>,
    session_id: Signal<String>,
    session_initialized: Signal<bool>,
    messages: Signal<Vec<Message>>,
    streaming: Signal<String>,
    input: Signal<String>,
    status: Signal<SendStatus>,
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
        anthropic_key,
        selected_persona,
        selected_model,
        session_id,
        mut session_initialized,
        mut messages,
        mut streaming,
        mut input,
        mut status,
    } = h;

    let user_text = input.peek().trim().to_string();
    if user_text.is_empty() {
        return;
    }
    let key = anthropic_key.peek().clone();
    let persona = selected_persona.peek().clone();
    let model = selected_model.peek().clone();
    let _ = model; // currently unused on the wire (proxy resolves model from persona)
    let sid = session_id.peek().clone();
    let already_init = *session_initialized.peek();

    if persona.is_empty() {
        status.set(SendStatus::Failed("select a persona first".into()));
        return;
    }
    if key.is_empty() {
        status.set(SendStatus::Failed(
            "set an Anthropic key in the field above".into(),
        ));
        return;
    }

    // Optimistically render the user turn and clear the composer.
    messages.with_mut(|m| {
        m.push(Message {
            role: Role::User,
            content: user_text.clone(),
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
                anthropic_key: Some(key.clone()),
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
            WireEvent::AiTurnAppended { .. } => {
                let final_text = streaming.peek().clone();
                streaming.set(String::new());
                messages.with_mut(|m| {
                    m.push(Message {
                        role: Role::Assistant,
                        content: final_text,
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
                    }
                }
                Err(e) => {
                    status.set(SendStatus::Failed(format!("auto-save build failed: {e}")));
                }
            }
        }
    });
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
