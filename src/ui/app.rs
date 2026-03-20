use std::cell::{Cell, RefCell};
use std::rc::Rc;

use dioxus::prelude::*;
use wasm_bindgen::JsCast;

use crate::audio::capture::BrowserCapture;
use crate::stt::assemblyai::{AssemblyAiSession, fetch_temp_token};

const TEXTAREA_ID: &str = "parley-transcript";

/// Read the textarea's selectionStart / selectionEnd from the DOM.
fn get_cursor() -> Option<(u32, u32)> {
    let doc = web_sys::window()?.document()?;
    let el = doc.get_element_by_id(TEXTAREA_ID)?;
    let ta: web_sys::HtmlTextAreaElement = el.dyn_into().ok()?;
    Some((ta.selection_start().ok()??, ta.selection_end().ok()??))
}

/// Schedule a microtask that restores the cursor position after Dioxus re-renders.
fn restore_cursor(start: u32, end: u32) {
    // Use queueMicrotask so we run after the virtual-DOM patch.
    let cb = wasm_bindgen::closure::Closure::once_into_js(move || {
        if let Some(window) = web_sys::window() {
            if let Some(doc) = window.document() {
                if let Some(el) = doc.get_element_by_id(TEXTAREA_ID) {
                    if let Ok(ta) = el.dyn_into::<web_sys::HtmlTextAreaElement>() {
                        let _ = ta.set_selection_start(Some(start));
                        let _ = ta.set_selection_end(Some(end));
                    }
                }
            }
        }
    });
    let _ = web_sys::window()
        .unwrap()
        .set_timeout_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), 0);
}

/// Scroll the transcript textarea to the bottom.
fn scroll_textarea_to_bottom() {
    let cb = wasm_bindgen::closure::Closure::once_into_js(move || {
        if let Some(window) = web_sys::window() {
            if let Some(doc) = window.document() {
                if let Some(el) = doc.get_element_by_id(TEXTAREA_ID) {
                    el.set_scroll_top(el.scroll_height());
                }
            }
        }
    });
    let _ = web_sys::window()
        .unwrap()
        .set_timeout_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), 0);
}

// ── Helpers ─────────────────────────────────────────────────────────
fn format_countdown(total_secs: u32) -> String {
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else if m > 0 {
        format!("{m}:{s:02}")
    } else {
        format!("0:{s:02}")
    }
}

fn load(key: &str) -> Option<String> {
    // Read from cookies (shared across all localhost ports).
    let doc = web_sys::window()?.document()?;
    let cookies = js_sys::Reflect::get(&doc, &"cookie".into())
        .ok()?
        .as_string()?;
    for pair in cookies.split(';') {
        let pair = pair.trim();
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn save(key: &str, value: &str) {
    // Store as cookie — max-age 10 years, SameSite=Strict, path=/.
    // Cookies ignore port, so these persist across localhost:8080, :8081, etc.
    if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
        let cookie = format!(
            "{}={}; path=/; max-age=315360000; SameSite=Strict",
            key, value
        );
        let _ = js_sys::Reflect::set(&doc, &"cookie".into(), &cookie.into());
    }
}

fn play_beep() {
    let ctx = match web_sys::AudioContext::new() {
        Ok(c) => c,
        Err(_) => return,
    };
    let osc = match ctx.create_oscillator() {
        Ok(o) => o,
        Err(_) => return,
    };
    osc.set_type(web_sys::OscillatorType::Sine);
    osc.frequency().set_value(880.0);
    let _ = osc.connect_with_audio_node(&ctx.destination());
    let _ = osc.start();
    let _ = osc.stop_with_when(ctx.current_time() + 0.3);
}

fn set_tab_title(title: &str) {
    if let Some(window) = web_sys::window() {
        if let Some(doc) = window.document() {
            doc.set_title(title);
        }
    }
}

// ── Formatting detection via proxy → Claude Haiku ────────────────────
/// Send the last unformatted chunk to Haiku.  If Haiku says formatting is
/// needed it returns the formatted text which we splice back into the
/// transcript.  Returns None if no change is needed or on error.
async fn check_formatting(anthropic_key: &str, full_transcript: &str) -> Option<String> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    // ── Parse transcript into "chunks" ──────────────────────────────
    // A chunk = a paragraph block + any list blocks immediately following it.
    // Split on blank lines first, then merge list blocks into the preceding chunk.
    let blocks: Vec<&str> = full_transcript.split("\n\n").collect();

    fn is_list_block(block: &str) -> bool {
        block.lines().all(|line| {
            let trimmed = line.trim();
            trimmed.starts_with("- ")
                || trimmed
                    .split_once(". ")
                    .map(|(num, _)| num.chars().all(|c| c.is_ascii_digit()))
                    .unwrap_or(false)
        })
    }

    // Group blocks into chunks: paragraph + trailing list blocks
    let mut chunks: Vec<String> = Vec::new();
    for block in &blocks {
        if chunks.is_empty() || !is_list_block(block) {
            chunks.push(block.to_string());
        } else {
            // Append list block to previous chunk
            let last = chunks.last_mut().unwrap();
            last.push_str("\n\n");
            last.push_str(block);
        }
    }

    let total = chunks.len();

    // Window: up to 3 chunks back. First 1 = context, last 2 = editable.
    let win = total.min(3);
    let win_start = total - win;
    let ctx_count = if win > 2 { win - 2 } else { 0 };
    let edit_start = win_start + ctx_count;

    let context_text = if ctx_count > 0 {
        chunks[win_start..win_start + ctx_count].join("\n\n")
    } else {
        String::new()
    };
    let editable_text = chunks[edit_start..].join("\n\n");

    // Everything before the window is frozen
    let prefix = if win_start > 0 {
        let mut p = chunks[..win_start].join("\n\n");
        p.push_str("\n\n");
        p
    } else {
        String::new()
    };

    if editable_text.trim().is_empty() {
        return None;
    }

    web_sys::console::log_1(
        &format!(
            "[parley] format check: {} chunks total, {} context chars, {} editable chars",
            total,
            context_text.len(),
            editable_text.len()
        )
        .into(),
    );

    let body = serde_json::json!({
        "anthropic_key": anthropic_key,
        "context": context_text,
        "text": editable_text,
    });

    let opts = web_sys::RequestInit::new();
    opts.set_method("POST");
    opts.set_mode(web_sys::RequestMode::Cors);
    let body_str = serde_json::to_string(&body).ok()?;
    opts.set_body(&wasm_bindgen::JsValue::from_str(&body_str));

    let headers = web_sys::Headers::new().ok()?;
    headers.set("Content-Type", "application/json").ok()?;
    opts.set_headers(&headers);

    let request =
        web_sys::Request::new_with_str_and_init("http://127.0.0.1:3033/format", &opts).ok()?;

    let window = web_sys::window()?;
    let resp_val = JsFuture::from(window.fetch_with_request(&request))
        .await
        .ok()?;
    let resp: web_sys::Response = resp_val.dyn_into().ok()?;

    if !resp.ok() {
        let err_body = JsFuture::from(resp.text().unwrap())
            .await
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default();
        web_sys::console::log_1(
            &format!("[parley] format proxy returned error: {err_body}").into(),
        );
        return None;
    }

    let json_val = JsFuture::from(resp.json().ok()?).await.ok()?;
    let parsed: serde_json::Value =
        serde_json::from_str(&js_sys::JSON::stringify(&json_val).ok()?.as_string()?).ok()?;

    let changed = parsed["changed"].as_bool().unwrap_or(false);
    if !changed {
        web_sys::console::log_1(&"[parley] Haiku says no formatting needed".into());
        return None;
    }

    let formatted_tail = parsed["formatted"].as_str()?.to_string();
    web_sys::console::log_1(&"[parley] Haiku applied formatting".into());

    // Splice back: frozen prefix + read-only context + formatted editable
    let mut result = prefix;
    if !context_text.is_empty() {
        result.push_str(&context_text);
        result.push_str("\n\n");
    }
    result.push_str(&formatted_tail);
    Some(result)
}

// ── State ───────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq)]
enum RecState {
    Idle,
    Recording,
    Stopped,
}

// ── Root component ──────────────────────────────────────────────────
#[component]
pub fn App() -> Element {
    // Signals
    let mut rec_state = use_signal(|| RecState::Idle);
    let mut transcript = use_signal(String::new);
    let mut partial = use_signal(String::new);
    let mut status_msg = use_signal(|| "Ready".to_string());
    let mut error_msg: Signal<Option<String>> = use_signal(|| None);
    let mut show_settings = use_signal(|| false);
    let mut api_key = use_signal(|| load("parley_api_key").unwrap_or_default());
    let mut idle_minutes = use_signal(|| {
        load("parley_idle_minutes")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(5)
    });
    let mut anthropic_key = use_signal(|| load("parley_anthropic_key").unwrap_or_default());
    let mut copied = use_signal(|| false);
    let mut countdown_secs: Signal<Option<u32>> = use_signal(|| None);

    // Shared handles for audio + stt session (kept in Rc<RefCell<>>)
    let capture_handle: Signal<Option<Rc<RefCell<Option<BrowserCapture>>>>> = use_signal(|| None);
    let session_handle: Signal<Option<Rc<RefCell<Option<AssemblyAiSession>>>>> =
        use_signal(|| None);

    // Expose turn-tracking Rc handles so on_clear can reset them
    let mut current_turn_shared: Signal<Option<Rc<RefCell<String>>>> = use_signal(|| None);
    let mut current_turn_order_shared: Signal<Option<Rc<Cell<u32>>>> = use_signal(|| None);
    let mut needs_paragraph_check_shared: Signal<Option<Rc<Cell<bool>>>> = use_signal(|| None);
    let mut auto_scroll = use_signal(|| true);

    // Auto-scroll the transcript textarea whenever its content changes
    use_effect(move || {
        let _ = (transcript)(); // subscribe to transcript changes
        if (auto_scroll)() {
            scroll_textarea_to_bottom();
        }
    });

    // ── Record (core logic) ───────────────────────────────────────
    let mut start_recording = move || {
        let key = (api_key)().trim().to_string();
        if key.is_empty() {
            error_msg.set(Some("Set your API key in Settings first.".into()));
            return;
        }
        error_msg.set(None);
        status_msg.set("Connecting…".into());
        rec_state.set(RecState::Recording);

        spawn(async move {
            // First, fetch a temporary streaming token from AssemblyAI.
            // The v2 real-time endpoint requires a temp token, not the raw API key.
            status_msg.set("Fetching token…".into());
            let token = match fetch_temp_token(&key).await {
                Ok(t) => t,
                Err(e) => {
                    error_msg.set(Some(format!("Token fetch failed: {e}")));
                    rec_state.set(RecState::Idle);
                    status_msg.set("Ready".into());
                    return;
                }
            };

            status_msg.set("Connecting…".into());
            let session_rc: Rc<RefCell<Option<AssemblyAiSession>>> = Rc::new(RefCell::new(None));

            // Idle timeout tracking: timestamp of last transcript event
            let last_activity = Rc::new(Cell::new(js_sys::Date::now()));

            let mut transcript_clone = transcript.clone();
            let mut partial_clone = partial.clone();
            let last_activity2 = last_activity.clone();

            // Track the current turn. Use turn_order from v3 to
            // reliably detect when a new turn starts.
            let current_turn: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
            let current_turn2 = current_turn.clone();
            let current_turn_order: Rc<Cell<u32>> = Rc::new(Cell::new(u32::MAX));
            let current_turn_order2 = current_turn_order.clone();
            let anthropic_key_val = (anthropic_key)();
            let anthropic_key_for_ticker = anthropic_key_val.clone();
            // Flag: when set to true, the ticker loop will fire a paragraph
            // detection request for the current transcript text.
            let needs_paragraph_check: Rc<Cell<bool>> = Rc::new(Cell::new(false));
            let needs_paragraph_check2 = needs_paragraph_check.clone();

            // Store Rc handles so on_clear can reset them
            current_turn_shared.set(Some(current_turn.clone()));
            current_turn_order_shared.set(Some(current_turn_order.clone()));
            needs_paragraph_check_shared.set(Some(needs_paragraph_check.clone()));

            let session = match AssemblyAiSession::connect(
                &token,
                move |text, _is_formatted, turn_order| {
                    last_activity2.set(js_sys::Date::now());
                    let text = text.replace('\n', " ").replace("  ", " ");

                    let prev_order = current_turn_order2.get();
                    let is_new_turn = turn_order != prev_order;

                    // New turn — commit old turn to transcript, then ask Haiku for paragraphs
                    if is_new_turn && prev_order != u32::MAX {
                        let old_turn = current_turn2.borrow().clone();
                        if !old_turn.is_empty() {
                            let cursor = get_cursor();
                            let prev = (transcript_clone)();
                            let sep = if prev.is_empty() { "" } else { " " };
                            let combined = format!("{prev}{sep}{old_turn}");
                            transcript_clone.set(combined.clone());
                            if let Some((s, e)) = cursor {
                                restore_cursor(s, e);
                            }
                            // Signal the ticker to fire paragraph detection
                            if !anthropic_key_val.is_empty() {
                                needs_paragraph_check2.set(true);
                            }
                        }
                    }

                    // Update bottom box with current turn text
                    current_turn_order2.set(turn_order);
                    *current_turn2.borrow_mut() = text.clone();
                    partial_clone.set(text);
                },
                {
                    let mut rec_state = rec_state.clone();
                    let mut status_msg = status_msg.clone();
                    let mut error_msg = error_msg.clone();
                    move |code: u16, reason: String| {
                        rec_state.set(RecState::Stopped);
                        if code == 4001 {
                            error_msg.set(Some("Not authorized — check your API key".into()));
                            status_msg.set("Auth error".into());
                        } else if code == 1006 {
                            let detail = if reason.is_empty() {
                                "connection failed".to_string()
                            } else {
                                reason
                            };
                            error_msg.set(Some(format!("WebSocket error: {detail}")));
                            status_msg.set("Error".into());
                        } else if code != 1000 {
                            error_msg
                                .set(Some(format!("Connection closed (code {code}): {reason}")));
                            status_msg.set("Disconnected".into());
                        } else {
                            status_msg.set("Disconnected".into());
                        }
                    }
                },
            ) {
                Ok(s) => s,
                Err(e) => {
                    error_msg.set(Some(format!("WS connect failed: {e:?}")));
                    rec_state.set(RecState::Idle);
                    status_msg.set("Ready".into());
                    return;
                }
            };

            *session_rc.borrow_mut() = Some(session);
            session_handle.clone().set(Some(session_rc.clone()));

            // 3. Start audio capture
            let session_rc2 = session_rc.clone();
            match BrowserCapture::start(move |samples| {
                if let Some(ref sess) = *session_rc2.borrow() {
                    let _ = sess.send_audio(&samples);
                }
            })
            .await
            {
                Ok(cap) => {
                    let cap_rc: Rc<RefCell<Option<BrowserCapture>>> =
                        Rc::new(RefCell::new(Some(cap)));
                    capture_handle.clone().set(Some(cap_rc.clone()));
                    status_msg.set("Recording…".into());

                    // 4. Spawn countdown ticker + idle timeout checker (1s interval)
                    countdown_secs.set(Some(idle_minutes() * 60));
                    let last_activity = last_activity.clone();
                    let session_for_timeout = session_rc.clone();
                    let cap_for_timeout = cap_rc;
                    let mut rec_state = rec_state.clone();
                    let mut status_msg = status_msg.clone();
                    let mut partial = partial.clone();
                    let mut transcript = transcript.clone();
                    let mut countdown_secs = countdown_secs.clone();
                    // Share paragraph-check flag + key for ticker
                    let ticker_needs_para = needs_paragraph_check.clone();
                    let ticker_anthropic_key = anthropic_key_for_ticker;
                    spawn(async move {
                        let mut blink_on = false;
                        let mut beep_cooldown: u32 = 0;
                        loop {
                            gloo_timers::future::TimeoutFuture::new(1_000).await;
                            if rec_state() != RecState::Recording {
                                countdown_secs.set(None);
                                set_tab_title("Parley");
                                break;
                            }
                            let timeout_total_secs = idle_minutes() * 60;
                            let elapsed_ms = js_sys::Date::now() - last_activity.get();
                            let timeout_ms = (timeout_total_secs as f64) * 1000.0;
                            let remaining_ms = (timeout_ms - elapsed_ms).max(0.0);
                            let remaining_secs = (remaining_ms / 1000.0).ceil() as u32;
                            countdown_secs.set(Some(remaining_secs));

                            // Check if the STT callback requested paragraph detection
                            if ticker_needs_para.get() {
                                ticker_needs_para.set(false);
                                let key = ticker_anthropic_key.clone();
                                let mut t = transcript.clone();
                                // Fire-and-forget: don't block the ticker
                                spawn(async move {
                                    let text = (t)();
                                    if !text.is_empty() {
                                        if let Some(formatted) = check_formatting(&key, &text).await
                                        {
                                            let cursor = get_cursor();
                                            t.set(formatted);
                                            if let Some((s, e)) = cursor {
                                                restore_cursor(s, e);
                                            }
                                        }
                                    }
                                });
                            }

                            // 1-minute warning: beep every 10s + tab blink
                            if remaining_secs <= 60 && remaining_secs > 0 {
                                if beep_cooldown == 0 {
                                    play_beep();
                                    beep_cooldown = 10;
                                } else {
                                    beep_cooldown -= 1;
                                }
                                blink_on = !blink_on;
                                set_tab_title(if blink_on {
                                    "\u{26a0} Parley \u{2014} time running out"
                                } else {
                                    "Parley"
                                });
                            } else if remaining_secs > 60 {
                                beep_cooldown = 0;
                                blink_on = false;
                                set_tab_title("Parley");
                            }

                            if remaining_secs == 0 {
                                // Auto-disconnect
                                set_tab_title("Parley");
                                if let Some(cap) = cap_for_timeout.borrow_mut().take() {
                                    cap.stop();
                                }
                                if let Some(ref sess) = *session_for_timeout.borrow() {
                                    let _ = sess.terminate();
                                }
                                let p = (partial)();
                                if !p.is_empty() {
                                    let prev = (transcript)();
                                    let sep = if prev.is_empty() { "" } else { " " };
                                    transcript.set(format!("{prev}{sep}{p}"));
                                    partial.set(String::new());
                                }
                                rec_state.set(RecState::Stopped);
                                status_msg.set("Idle timeout \u{2014} disconnected".into());
                                countdown_secs.set(None);
                                break;
                            }
                        }
                    });
                }
                Err(e) => {
                    error_msg.set(Some(format!("Mic access denied: {e:?}")));
                    // Clean up session
                    if let Some(ref sess) = *session_rc.borrow() {
                        let _ = sess.terminate();
                    }
                    rec_state.set(RecState::Idle);
                    status_msg.set("Ready".into());
                }
            }
        });
    };

    let on_record = move |_: Event<MouseData>| {
        start_recording();
    };

    // ── End Turn (force-commit current turn without stopping) ────
    let on_end_turn = move |_| {
        if let Some(sess_rc) = (session_handle)().as_ref() {
            if let Some(ref sess) = *sess_rc.borrow() {
                let _ = sess.force_endpoint();
            }
        }
        let p = (partial)();
        if !p.is_empty() {
            let prev = (transcript)();
            let sep = if prev.is_empty() { "" } else { " " };
            transcript.set(format!("{prev}{sep}{p}"));
            partial.set(String::new());
        }
        // Reset shared turn state so the callback treats the next event as a fresh turn
        if let Some(ct) = (current_turn_shared)().as_ref() {
            *ct.borrow_mut() = String::new();
        }
        if let Some(cto) = (current_turn_order_shared)().as_ref() {
            cto.set(u32::MAX);
        }
    };

    // ── Stop ────────────────────────────────────────────────────────
    let on_stop = move |_| {
        // Stop audio capture
        if let Some(cap_rc) = (capture_handle)().as_ref() {
            if let Some(cap) = cap_rc.borrow_mut().take() {
                cap.stop();
            }
        }
        // Terminate session
        if let Some(sess_rc) = (session_handle)().as_ref() {
            if let Some(ref sess) = *sess_rc.borrow() {
                let _ = sess.terminate();
            }
        }
        // Flush partial into transcript
        let p = (partial)();
        if !p.is_empty() {
            let prev = (transcript)();
            let sep = if prev.is_empty() { "" } else { " " };
            transcript.set(format!("{prev}{sep}{p}"));
            partial.set(String::new());
        }
        // Run paragraph detection on the final text
        let akey = (anthropic_key)();
        if !akey.is_empty() {
            let mut t = transcript.clone();
            spawn(async move {
                let text = (t)();
                if !text.is_empty() {
                    if let Some(formatted) = check_formatting(&akey, &text).await {
                        let cursor = get_cursor();
                        t.set(formatted);
                        if let Some((s, e)) = cursor {
                            restore_cursor(s, e);
                        }
                    }
                }
            });
        }
        rec_state.set(RecState::Stopped);
        status_msg.set("Stopped".into());
    };

    // ── Continue ────────────────────────────────────────────────────
    let on_continue = move |_: Event<MouseData>| {
        start_recording();
    };

    // ── Copy ────────────────────────────────────────────────────────
    let on_copy = move |_| {
        let text = (transcript)();
        if text.is_empty() {
            return;
        }
        if let Some(window) = web_sys::window() {
            let clipboard = window.navigator().clipboard();
            {
                let _ = clipboard.write_text(&text);
                copied.set(true);
                // Reset after 2s
                spawn(async move {
                    gloo_timers::future::TimeoutFuture::new(2_000).await;
                    copied.set(false);
                });
            }
        }
    };

    // ── Clear ───────────────────────────────────────────────────────
    let on_clear = move |_| {
        transcript.set(String::new());
        partial.set(String::new());
        // Reset shared turn state so stale turns don't leak into transcript
        if let Some(ct) = (current_turn_shared)().as_ref() {
            *ct.borrow_mut() = String::new();
        }
        if let Some(cto) = (current_turn_order_shared)().as_ref() {
            cto.set(u32::MAX);
        }
        if let Some(npc) = (needs_paragraph_check_shared)().as_ref() {
            npc.set(false);
        }
        if rec_state() == RecState::Stopped {
            rec_state.set(RecState::Idle);
            status_msg.set("Ready".into());
        }
    };

    // ── Derived values ──────────────────────────────────────────────
    let state = rec_state();
    let has_text = !(transcript)().is_empty() || !(partial)().is_empty();

    // ── Render ──────────────────────────────────────────────────────
    rsx! {
        style { {CSS} }
        div { class: "parley-root",
            // ── Header ──────────────────────────────────────────────
            div { class: "header",
                h1 { class: "title", "Parley" }
                if let Some(secs) = (countdown_secs)() {
                    div { class: if secs <= 120 { "countdown countdown-warn" } else { "countdown" },
                        "{format_countdown(secs)}"
                    }
                }
                button {
                    class: "gear-btn",
                    title: "Settings",
                    onclick: move |_| show_settings.set(!show_settings()),
                    "⚙️"
                }
            }

            // ── Error banner ────────────────────────────────────────
            if let Some(ref err) = (error_msg)() {
                div { class: "error-banner",
                    span { "{err}" }
                    button {
                        class: "error-dismiss",
                        onclick: move |_| error_msg.set(None),
                        "✕"
                    }
                }
            }

            // ── Finalized transcript (editable) ────────────────────
            div { class: "transcript-area",
                textarea {
                    id: TEXTAREA_ID,
                    class: "transcript-edit",
                    placeholder: "Transcribed text will appear here…",
                    value: "{transcript}",
                    oninput: move |evt: Event<FormData>| {
                        transcript.set(evt.value());
                    },
                }
                label { class: "auto-scroll-toggle",
                    input {
                        r#type: "checkbox",
                        checked: "{auto_scroll}",
                        onchange: move |evt: Event<FormData>| {
                            auto_scroll.set(evt.checked());
                        },
                    }
                    "Auto-scroll"
                }
            }

            // ── Current turn (read-only, always visible) ─────────────
            div { class: "current-turn",
                if state == RecState::Recording {
                    span { class: "current-turn-label", "Speaking…" }
                    p { class: "current-turn-text",
                        if (partial)().is_empty() {
                            "…"
                        } else {
                            "{partial}"
                        }
                    }
                } else {
                    span { class: "current-turn-label current-turn-idle", "Current turn" }
                    p { class: "current-turn-text current-turn-placeholder", "Not recording" }
                }
            }

            // ── Button bar ──────────────────────────────────────────
            div { class: "button-bar",
                // Record (visible when idle, or stopped with no text)
                if state == RecState::Idle || (state == RecState::Stopped && !has_text) {
                    button { class: "btn btn-record", onclick: on_record, "● Record" }
                }
                // Stop (visible when recording)
                if state == RecState::Recording {
                    button { class: "btn btn-stop", onclick: on_stop, "■ Stop" }
                    button { class: "btn btn-endturn", onclick: on_end_turn, "⏎ End Turn" }
                }
                // Continue (visible when stopped and has text)
                if state == RecState::Stopped && has_text {
                    button { class: "btn btn-continue", onclick: on_continue, "● Continue" }
                }
                // Copy
                if has_text {
                    button { class: "btn btn-copy", onclick: on_copy,
                        if copied() {
                            "✓ Copied"
                        } else {
                            "Copy"
                        }
                    }
                }
                // Clear
                if has_text {
                    button { class: "btn btn-clear", onclick: on_clear, "Clear" }
                }
            }

            // ── Status bar ──────────────────────────────────────────
            div { class: "status-bar",
                span { class: if state == RecState::Recording { "status-dot recording" } else { "status-dot" } }
                span { "{status_msg}" }
            }

            // ── Settings drawer ─────────────────────────────────────
            if show_settings() {
                div {
                    class: "settings-overlay",
                    onclick: move |_| show_settings.set(false),
                }
                div { class: "settings-drawer",
                    h2 { "Settings" }

                    label { r#for: "api-key", "AssemblyAI API Key" }
                    input {
                        id: "api-key",
                        r#type: "password",
                        class: "settings-input",
                        placeholder: "Enter your API key…",
                        value: "{api_key}",
                        oninput: move |evt: Event<FormData>| {
                            let val = evt.value();
                            api_key.set(val.clone());
                            save("parley_api_key", &val);
                        },
                    }

                    label { r#for: "idle-timeout", "Idle timeout (minutes)" }
                    input {
                        id: "idle-timeout",
                        r#type: "number",
                        class: "settings-input",
                        min: "1",
                        max: "60",
                        value: "{idle_minutes}",
                        oninput: move |evt: Event<FormData>| {
                            if let Ok(v) = evt.value().parse::<u32>() {
                                idle_minutes.set(v);
                                save("parley_idle_minutes", &v.to_string());
                            }
                        },
                    }

                    p { class: "settings-hint",
                        "Auto-disconnect after this many minutes of silence to save costs."
                    }

                    label { r#for: "anthropic-key", "Anthropic API Key (paragraph detection)" }
                    input {
                        id: "anthropic-key",
                        r#type: "password",
                        class: "settings-input",
                        placeholder: "sk-ant-... (optional)",
                        value: "{anthropic_key}",
                        oninput: move |evt: Event<FormData>| {
                            let val = evt.value();
                            anthropic_key.set(val.clone());
                            save("parley_anthropic_key", &val);
                        },
                    }

                    p { class: "settings-hint",
                        "If set, Claude Haiku will automatically detect paragraph breaks in your transcript."
                    }

                    button {
                        class: "btn btn-close-settings",
                        onclick: move |_| show_settings.set(false),
                        "Close"
                    }
                }
            }
        }
    }
}

// ── CSS ─────────────────────────────────────────────────────────────
const CSS: &str = r#"
* { margin: 0; padding: 0; box-sizing: border-box; }

body {
    background: #1a1a2e;
    color: #e0e0e0;
    font-family: 'Inter', system-ui, -apple-system, sans-serif;
    min-height: 100vh;
}

.parley-root {
    max-width: 720px;
    margin: 0 auto;
    padding: 2rem 1.5rem;
    display: flex;
    flex-direction: column;
    min-height: 100vh;
}

/* Header */
.header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 1.5rem;
}
.title {
    font-size: 1.8rem;
    font-weight: 700;
    letter-spacing: -0.02em;
    color: #e0e0e0;
}
.gear-btn {
    background: none;
    border: none;
    font-size: 1.8rem;
    color: #8888aa;
    cursor: pointer;
    padding: 0.3rem;
    border-radius: 6px;
    transition: color 0.15s, background 0.15s;
    line-height: 1;
}
.gear-btn:hover { color: #e0e0e0; background: #16213e; }

/* Countdown (inline in header) */
.countdown {
    font-size: 1.3rem;
    font-weight: 600;
    font-variant-numeric: tabular-nums;
    color: #4ecca3;
    letter-spacing: 0.04em;
}
.countdown-warn {
    color: #e94560;
    animation: blink 1s steps(1) infinite;
}
@keyframes blink {
    0%, 100% { opacity: 1; }
    50% { opacity: 0.3; }
}

/* Error banner */
.error-banner {
    background: #3a1020;
    border: 1px solid #e94560;
    border-radius: 8px;
    padding: 0.75rem 1rem;
    margin-bottom: 1rem;
    display: flex;
    justify-content: space-between;
    align-items: center;
    color: #ff6b81;
    font-size: 0.9rem;
}
.error-dismiss {
    background: none;
    border: none;
    color: #ff6b81;
    cursor: pointer;
    font-size: 1.1rem;
    padding: 0 0.3rem;
}

/* Transcript area */
.transcript-area {
    flex: 1;
    display: flex;
    flex-direction: column;
    background: #16213e;
    border-radius: 12px;
    padding: 1.5rem;
    margin-bottom: 1.5rem;
    min-height: 300px;
    max-height: 60vh;
    overflow-y: auto;
    line-height: 1.7;
    font-size: 1.05rem;
}
.placeholder {
    color: #8888aa;
    font-style: italic;
}
.transcript-edit {
    flex: 1;
    width: 100%;
    min-height: 260px;
    background: transparent;
    color: #e0e0e0;
    border: none;
    outline: none;
    resize: vertical;
    font-family: inherit;
    font-size: inherit;
    line-height: inherit;
    cursor: text;
}
.final-text p {
    margin-bottom: 0.8rem;
}
.partial-text {
    color: #8888aa;
    font-style: italic;
    border-left: 3px solid #0f3460;
    padding-left: 0.8rem;
    margin-top: 0.5rem;
}

/* Current turn box */
.current-turn {
    background: #0f3460;
    border-radius: 10px;
    padding: 1rem 1.25rem;
    margin-bottom: 1.5rem;
    line-height: 1.7;
    font-size: 1.05rem;
    border-left: 3px solid #4ecca3;
    max-height: 30vh;
    overflow-y: auto;
}
.current-turn-label {
    display: block;
    font-size: 0.75rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: #4ecca3;
    margin-bottom: 0.4rem;
}
.current-turn-text {
    color: #c0c0d0;
    font-style: italic;
}
.current-turn-idle {
    color: #8888aa;
}
.current-turn-placeholder {
    color: #555570;
}

/* Auto-scroll toggle */
.auto-scroll-toggle {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.8rem;
    color: #8888aa;
    margin-top: 0.5rem;
    cursor: pointer;
    user-select: none;
}
.auto-scroll-toggle input {
    accent-color: #4ecca3;
    cursor: pointer;
}

/* Button bar */
.button-bar {
    display: flex;
    gap: 0.75rem;
    margin-bottom: 1rem;
    flex-wrap: wrap;
}
.btn {
    padding: 0.65rem 1.4rem;
    border: none;
    border-radius: 8px;
    font-size: 0.95rem;
    font-weight: 600;
    cursor: pointer;
    transition: background 0.15s, transform 0.1s;
}
.btn:active { transform: scale(0.97); }

.btn-record { background: #e94560; color: #fff; }
.btn-record:hover { background: #ff6b81; }

.btn-stop { background: #e94560; color: #fff; }
.btn-stop:hover { background: #ff6b81; }

.btn-continue { background: #4ecca3; color: #1a1a2e; }
.btn-continue:hover { background: #6ee6bb; }

.btn-copy { background: #0f3460; color: #e0e0e0; }
.btn-copy:hover { background: #1a4a7a; }

.btn-clear { background: transparent; color: #8888aa; border: 1px solid #8888aa; }
.btn-clear:hover { color: #e0e0e0; border-color: #e0e0e0; }

/* Status bar */
.status-bar {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    font-size: 0.85rem;
    color: #8888aa;
}
.status-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: #8888aa;
}
.status-dot.recording {
    background: #e94560;
    animation: pulse 1.2s infinite;
}
@keyframes pulse {
    0%, 100% { opacity: 1; }
    50% { opacity: 0.4; }
}

/* Settings overlay + drawer */
.settings-overlay {
    position: fixed;
    top: 0; left: 0; right: 0; bottom: 0;
    background: rgba(0,0,0,0.5);
    z-index: 99;
}
.settings-drawer {
    position: fixed;
    top: 0;
    right: 0;
    width: 340px;
    max-width: 90vw;
    height: 100vh;
    background: #16213e;
    z-index: 100;
    padding: 2rem 1.5rem;
    overflow-y: auto;
    box-shadow: -4px 0 20px rgba(0,0,0,0.4);
}
.settings-drawer h2 {
    margin-bottom: 1.5rem;
    font-size: 1.3rem;
    color: #e0e0e0;
}
.settings-drawer label {
    display: block;
    font-size: 0.85rem;
    color: #8888aa;
    margin-bottom: 0.4rem;
    margin-top: 1.2rem;
}
.settings-input {
    width: 100%;
    padding: 0.6rem 0.8rem;
    border: 1px solid #0f3460;
    border-radius: 6px;
    background: #1a1a2e;
    color: #e0e0e0;
    font-size: 0.95rem;
    outline: none;
    transition: border-color 0.15s;
}
.settings-input:focus { border-color: #4ecca3; }
.settings-hint {
    font-size: 0.8rem;
    color: #8888aa;
    margin-top: 0.5rem;
}
.btn-close-settings {
    margin-top: 2rem;
    background: #0f3460;
    color: #e0e0e0;
    width: 100%;
}
.btn-close-settings:hover { background: #1a4a7a; }
"#;
