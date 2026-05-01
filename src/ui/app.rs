use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::future::Future;
use std::rc::Rc;

use dioxus::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use crate::audio::capture::BrowserCapture;
use crate::stt::assemblyai::{AssemblyAiSession, TurnEvent, fetch_temp_token};
use crate::stt::soniox::{SonioxConfig, SonioxLatencyMode, SonioxSession, fetch_temp_api_key};
use crate::ui::secrets;
use parley_core::stt::{SttStreamEvent, TokenStreamNormalizer};
use parley_core::word_graph::SttWord;
use parley_core::word_graph::WordGraph;

const TEXTAREA_ID: &str = "parley-transcript";
const CLEAR_CONFIRM_CANCEL_ID: &str = "clear-confirm-cancel";
const AUTO_FORMAT_CHAR_LIMIT: usize = 500;

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
        if let Some(window) = web_sys::window()
            && let Some(doc) = window.document()
            && let Some(el) = doc.get_element_by_id(TEXTAREA_ID)
            && let Ok(ta) = el.dyn_into::<web_sys::HtmlTextAreaElement>()
        {
            let _ = ta.set_selection_start(Some(start));
            let _ = ta.set_selection_end(Some(end));
        }
    });
    let _ = web_sys::window()
        .unwrap()
        .set_timeout_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), 0);
}

/// Scroll the transcript textarea to the bottom.
fn scroll_textarea_to_bottom() {
    let cb = wasm_bindgen::closure::Closure::once_into_js(move || {
        if let Some(window) = web_sys::window()
            && let Some(doc) = window.document()
            && let Some(el) = doc.get_element_by_id(TEXTAREA_ID)
        {
            el.set_scroll_top(el.scroll_height());
        }
    });
    let _ = web_sys::window()
        .unwrap()
        .set_timeout_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), 0);
}

fn focus_clear_confirm_cancel() {
    if let Some(window) = web_sys::window()
        && let Some(doc) = window.document()
        && let Some(el) = doc.get_element_by_id(CLEAR_CONFIRM_CANCEL_ID)
        && let Ok(button) = el.dyn_into::<web_sys::HtmlElement>()
    {
        let _ = button.focus();
    }
}

fn defer_ui_update(update: impl FnOnce() + 'static) {
    gloo_timers::callback::Timeout::new(0, update).forget();
}

fn spawn_browser_task(future: impl Future<Output = ()> + 'static) {
    spawn_local(future);
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

pub(crate) fn load(key: &str) -> Option<String> {
    // Read from cookies (shared across all localhost ports).
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

pub(crate) fn save(key: &str, value: &str) {
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

pub(crate) fn load_local(key: &str) -> Option<String> {
    web_sys::window()?
        .local_storage()
        .ok()??
        .get_item(key)
        .ok()?
}

pub(crate) fn save_local(key: &str, value: &str) {
    if let Some(storage) = web_sys::window()
        .and_then(|window| window.local_storage().ok())
        .flatten()
    {
        let _ = storage.set_item(key, value);
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
    if let Some(window) = web_sys::window()
        && let Some(doc) = window.document()
    {
        doc.set_title(title);
    }
}

/// Result from a formatting check, including optional token usage for cost tracking.
struct FormatResult {
    formatted: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
}

// ── Formatting detection via proxy → Claude Haiku ────────────────────
/// Send the last unformatted chunk to Haiku.  If Haiku says formatting is
/// needed it returns the formatted text which we splice back into the
/// transcript.  Returns None if no change is needed or on error.
///
/// Authentication: the proxy resolves its Anthropic key from the OS
/// keystore via the proxy's `SecretsManager`. The browser never sends
/// a key on this request — a `412 Precondition Failed` from the proxy
/// indicates the user hasn't configured one in Settings yet, which we
/// surface as a `None` (caller treats this as "no change").
async fn check_formatting(
    full_transcript: &str,
    multi_speaker: bool,
    model_config_id: &str,
    credential: &str,
    depth: usize,
    context_depth: usize,
) -> Option<FormatResult> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    // No reformat model picked yet: nothing to do. The drawer
    // populates this once /models loads.
    if model_config_id.is_empty() {
        return None;
    }

    // ── Full-transcript mode (depth == 0) ───────────────────────────
    if depth == 0 {
        if full_transcript.trim().is_empty() {
            return None;
        }

        web_sys::console::log_1(
            &format!(
                "[parley] format check (full): {} chars, model_config_id={}",
                full_transcript.len(),
                model_config_id,
            )
            .into(),
        );

        let body = serde_json::json!({
            "context": "",
            "text": full_transcript,
            "multi_speaker": multi_speaker,
            "model_config_id": model_config_id,
            "credential": credential,
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
            return None;
        }

        let json_val = JsFuture::from(resp.json().ok()?).await.ok()?;
        let parsed: serde_json::Value =
            serde_json::from_str(&js_sys::JSON::stringify(&json_val).ok()?.as_string()?).ok()?;

        let input_tokens = parsed["input_tokens"].as_u64().unwrap_or(0);
        let output_tokens = parsed["output_tokens"].as_u64().unwrap_or(0);
        let changed = parsed["changed"].as_bool().unwrap_or(false);
        let formatted_len = parsed["formatted"].as_str().map(str::len).unwrap_or(0);
        let formatted_has_break = parsed["formatted"]
            .as_str()
            .map(|formatted| formatted.contains("\n\n"))
            .unwrap_or(false);
        web_sys::console::log_1(
            &format!(
                "[parley] Reformat response: changed={changed}, formatted_len={formatted_len}, formatted_has_break={formatted_has_break}, input_tokens={input_tokens}, output_tokens={output_tokens}",
            )
            .into(),
        );

        if !changed {
            web_sys::console::log_1(&"[parley] Reformat: no changes needed".into());
            return Some(FormatResult {
                formatted: None,
                input_tokens,
                output_tokens,
            });
        }

        let formatted = parsed["formatted"].as_str()?.to_string();
        web_sys::console::log_1(&"[parley] Reformat: applied formatting".into());
        return Some(FormatResult {
            formatted: Some(formatted),
            input_tokens,
            output_tokens,
        });
    }

    // ── Windowed mode (depth > 0) ───────────────────────────────────
    // A chunk = a paragraph block + any list blocks immediately following it.
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
            let last = chunks.last_mut().unwrap();
            last.push_str("\n\n");
            last.push_str(block);
        }
    }

    let total = chunks.len();

    // Window size: editable = depth, context padding is user-configurable
    let context_padding: usize = context_depth;
    let max_editable = depth;
    let max_win = depth + context_padding;
    let win = total.min(max_win);
    let win_start = total - win;
    let editable = win.min(max_editable);
    let ctx_count = win - editable;
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
            "[parley] format check: {} chunks total, {} context chars, {} editable chars, model_config_id={}",
            total,
            context_text.len(),
            editable_text.len(),
            model_config_id,
        )
        .into(),
    );

    let body = serde_json::json!({
        "context": context_text,
        "text": editable_text,
        "multi_speaker": multi_speaker,
        "model_config_id": model_config_id,
        "credential": credential,
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

    let input_tokens = parsed["input_tokens"].as_u64().unwrap_or(0);
    let output_tokens = parsed["output_tokens"].as_u64().unwrap_or(0);

    let changed = parsed["changed"].as_bool().unwrap_or(false);
    if !changed {
        web_sys::console::log_1(&"[parley] Haiku says no formatting needed".into());
        return Some(FormatResult {
            formatted: None,
            input_tokens,
            output_tokens,
        });
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
    Some(FormatResult {
        formatted: Some(result),
        input_tokens,
        output_tokens,
    })
}

fn has_sentence_marker(text: &str) -> bool {
    text.chars()
        .any(|ch| matches!(ch, '.' | '?' | '!' | '\u{3002}' | '\u{ff1f}' | '\u{ff01}'))
}

/// Note newly committed STT text and kick off a `/format` round-trip once the
/// stream reaches either a sentence marker or a punctuation-free character cap.
///
/// Called by every Capture Mode STT ingest path after text has actually
/// been committed to the transcript, so provider-specific turn shapes
/// still feed one formatter trigger policy. Punctuation is the preferred
/// boundary; the character cap is the provider-agnostic fallback for STT
/// streams that finalize words without punctuation.
///
/// All inputs are `Copy` (signals + `Rc`), so the helper is cheap to
/// call from inside an STT callback. The actual format request runs
/// on a spawned task and never blocks the audio pipeline.
#[allow(clippy::too_many_arguments)]
fn note_committed_text_and_maybe_format(
    sentence_counter: &Rc<Cell<u32>>,
    pending_chars: &Rc<Cell<usize>>,
    committed_text: &str,
    anthropic_configured: Memo<bool>,
    auto_format_enabled: Signal<bool>,
    format_nth: Signal<u32>,
    reformat_model_config_id: Signal<String>,
    reformat_credential: Signal<String>,
    format_depth: Signal<usize>,
    format_context_depth: Signal<usize>,
    mut transcript: Signal<String>,
    mut llm_cost: Signal<f64>,
    multi_speaker: bool,
) {
    if !*anthropic_configured.peek() || !*auto_format_enabled.peek() {
        return;
    }
    let added_chars = committed_text.trim().chars().count();
    if added_chars == 0 {
        return;
    }

    let chars_since_format = pending_chars.get() + added_chars;
    pending_chars.set(chars_since_format);

    let nth = (*format_nth.peek()).max(1);
    let sentence_ready = if has_sentence_marker(committed_text) {
        sentence_counter.set(sentence_counter.get() + 1);
        sentence_counter.get().is_multiple_of(nth)
    } else {
        false
    };
    let char_limit_ready = chars_since_format >= AUTO_FORMAT_CHAR_LIMIT;
    if !sentence_ready && !char_limit_ready {
        return;
    }
    let model = reformat_model_config_id.peek().clone();
    if model.is_empty() {
        return;
    }
    pending_chars.set(0);
    web_sys::console::log_1(
        &format!(
            "[parley] Auto-format scheduled ({chars_since_format} chars since last pass, sentence_ready={sentence_ready}, char_limit_ready={char_limit_ready})",
        )
        .into(),
    );
    let cred = reformat_credential.peek().clone();
    let depth = *format_depth.peek();
    let ctx_depth = *format_context_depth.peek();
    spawn_browser_task(async move {
        let text = transcript.peek().clone();
        if text.is_empty() {
            return;
        }
        if let Some(result) =
            check_formatting(&text, multi_speaker, &model, &cred, depth, ctx_depth).await
        {
            let (in_rate, out_rate) = llm_rates(&model);
            let previous_cost = *llm_cost.peek();
            llm_cost.set(
                previous_cost
                    + token_cost(result.input_tokens, result.output_tokens, in_rate, out_rate),
            );
            if let Some(formatted) = result.formatted {
                let cursor = get_cursor();
                let current = transcript.peek().clone();
                if current == text {
                    transcript.set(formatted);
                } else if let Some(suffix) = current.strip_prefix(&text) {
                    transcript.set(format!("{formatted}{suffix}"));
                } else {
                    web_sys::console::log_1(
                        &"[parley] Auto-format result skipped because transcript changed shape"
                            .into(),
                    );
                }
                if let Some((s, e)) = cursor {
                    restore_cursor(s, e);
                }
            }
        }
    });
}

// ── Cost helpers ────────────────────────────────────────────────────
/// Return (input_rate, output_rate) in $/token for the given Anthropic model ID.
fn llm_rates(model: &str) -> (f64, f64) {
    if model.contains("sonnet") {
        // Sonnet 4.6: $3/MTok input, $15/MTok output
        (3.0 / 1_000_000.0, 15.0 / 1_000_000.0)
    } else {
        // Haiku 4.5: $1/MTok input, $5/MTok output
        (1.0 / 1_000_000.0, 5.0 / 1_000_000.0)
    }
}

/// Compute dollar cost from token counts and per-token rates.
fn token_cost(input_tokens: u64, output_tokens: u64, in_rate: f64, out_rate: f64) -> f64 {
    (input_tokens as f64) * in_rate + (output_tokens as f64) * out_rate
}

/// Format a dollar amount for display. Shows 4 decimal places, or
/// fewer if the amount is large enough.
fn format_cost(dollars: f64) -> String {
    if dollars < 0.01 {
        format!("{:.4}", dollars)
    } else if dollars < 1.0 {
        format!("{:.3}", dollars)
    } else {
        format!("{:.2}", dollars)
    }
}

// ── State ───────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq)]
enum RecState {
    Idle,
    Recording,
    Stopping,
    Stopped,
}

#[derive(Clone, Copy, PartialEq)]
enum TransferMode {
    Copy,
    TxtFile,
    MdFile,
    VsCode,
}

impl TransferMode {
    fn label(&self) -> &'static str {
        match self {
            Self::Copy => "Copy",
            Self::TxtFile => "TXT File",
            Self::MdFile => "MD File",
            Self::VsCode => "VS Code",
        }
    }
    fn cookie_value(&self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::TxtFile => "txt",
            Self::MdFile => "md",
            Self::VsCode => "vscode",
        }
    }
    fn from_cookie(s: &str) -> Self {
        match s {
            "txt" => Self::TxtFile,
            "md" => Self::MdFile,
            "vscode" => Self::VsCode,
            _ => Self::Copy,
        }
    }
}

// (FormatTrigger enum removed — replaced by auto_format_enabled bool signal)

fn format_timestamp(elapsed_ms: f64) -> String {
    let total_secs = (elapsed_ms / 1000.0) as u32;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 {
        format!("[{}:{:02}:{:02}]", h, m, s)
    } else {
        format!("[{:02}:{:02}]", m, s)
    }
}

fn format_duration(elapsed_ms: f64) -> String {
    let total_secs = (elapsed_ms / 1000.0) as u32;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{}:{:02}", m, s)
    }
}

fn render_stt_words(words: &[SttWord]) -> String {
    let mut text = String::new();
    for word in words {
        let is_punctuation = matches!(word.text.as_str(), "." | "," | "?" | "!" | ";" | ":" | "\"");
        if !text.is_empty() && !is_punctuation {
            text.push(' ');
        }
        text.push_str(&word.text);
    }
    text
}

fn soniox_lane_name(lane: u8, speaker1_name: &str, speaker2_name: &str) -> String {
    match lane {
        0 => speaker1_name.to_string(),
        1 => speaker2_name.to_string(),
        lane => format!("Speaker {}", lane + 1),
    }
}

pub(crate) fn soniox_latency_mode_from_value(value: &str) -> SonioxLatencyMode {
    SonioxLatencyMode::from_storage_value(value).unwrap_or_default()
}

fn finalize_soniox_session_after_settle(
    session: Rc<RefCell<Option<SonioxSession>>>,
    mode: SonioxLatencyMode,
) {
    spawn_browser_task(async move {
        let settle_ms = mode.finalize_settle_ms();
        if settle_ms > 0 {
            gloo_timers::future::TimeoutFuture::new(settle_ms).await;
        }
        if let Some(ref session) = *session.borrow() {
            let _ = session.finalize();
        }
    });
}

fn trigger_download(filename: &str, content: &str, mime_type: &str) {
    let window = match web_sys::window() {
        Some(w) => w,
        None => return,
    };
    let document = match window.document() {
        Some(d) => d,
        None => return,
    };
    let parts = js_sys::Array::new();
    parts.push(&wasm_bindgen::JsValue::from_str(content));
    let options = web_sys::BlobPropertyBag::new();
    options.set_type(mime_type);
    let blob = match web_sys::Blob::new_with_str_sequence_and_options(&parts, &options) {
        Ok(b) => b,
        Err(_) => return,
    };
    let url = match web_sys::Url::create_object_url_with_blob(&blob) {
        Ok(u) => u,
        Err(_) => return,
    };
    if let Ok(el) = document.create_element("a") {
        let a: web_sys::HtmlAnchorElement = el.unchecked_into();
        a.set_href(&url);
        a.set_download(filename);
        a.click();
    }
    let _ = web_sys::Url::revoke_object_url(&url);
}

fn generate_filename(ext: &str) -> String {
    let date = js_sys::Date::new_0();
    let y = date.get_full_year();
    let mo = date.get_month() + 1;
    let d = date.get_date();
    let h = date.get_hours();
    let mi = date.get_minutes();
    let s = date.get_seconds();
    format!("parley-{y:04}-{mo:02}-{d:02}-{h:02}{mi:02}{s:02}.{ext}")
}

/// Estimated milliseconds per word at conversational pace (~170 wpm).
const MS_PER_WORD: f64 = 350.0;

/// A single word in the live zone with an estimated timestamp.
#[derive(Clone)]
struct LiveWord {
    estimated_ms: f64,
    speaker: String,
    word: String,
}

/// Split a completed turn into word-level entries with estimated timestamps.
fn split_turn_to_words(speech_start_ms: f64, speaker: &str, text: &str) -> Vec<LiveWord> {
    text.split_whitespace()
        .enumerate()
        .map(|(i, w)| LiveWord {
            estimated_ms: speech_start_ms + (i as f64) * MS_PER_WORD,
            speaker: speaker.to_string(),
            word: w.to_string(),
        })
        .collect()
}

/// Group consecutive same-speaker words into display lines for the live zone.
fn render_live_zone(words: &[LiveWord], show_labels: bool, show_timestamps: bool) -> Vec<String> {
    let mut lines = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let speaker = &words[i].speaker;
        let start_ms = words[i].estimated_ms;
        let mut phrase = String::new();
        while i < words.len() && words[i].speaker == *speaker {
            if !phrase.is_empty() {
                phrase.push(' ');
            }
            phrase.push_str(&words[i].word);
            i += 1;
        }
        let mut line = String::new();
        if show_timestamps {
            line.push_str(&format_timestamp(start_ms));
            line.push(' ');
        }
        if show_labels {
            line.push_str(&format!("[{}] ", speaker));
        }
        line.push_str(&phrase);
        lines.push(line);
    }
    lines
}

/// Build the committed line with optional timestamp and speaker label.
/// Returns the new full transcript string.
#[allow(clippy::too_many_arguments)]
fn build_committed_text(
    prev_transcript: &str,
    turn_text: &str,
    speaker_name: &str,
    multi_speaker: bool,
    show_labels: bool,
    show_timestamps: bool,
    elapsed_ms: f64,
    last_speaker_name: &str,
) -> String {
    let same_speaker = !last_speaker_name.is_empty() && last_speaker_name == speaker_name;
    let mut line = String::new();
    // Only add tags/timestamps at speaker transitions, not mid-paragraph
    if !same_speaker {
        if show_timestamps && multi_speaker {
            line.push_str(&format_timestamp(elapsed_ms));
            line.push(' ');
        }
        if show_labels && multi_speaker {
            line.push_str(&format!("[{}] ", speaker_name));
        }
    }
    line.push_str(turn_text);
    let sep = if prev_transcript.is_empty() {
        ""
    } else if multi_speaker && !same_speaker {
        "\n\n"
    } else {
        " "
    };
    format!("{prev_transcript}{sep}{line}")
}

/// Graduate safe words from the live zone into the transcript.
/// A word is safe when both speakers' current turn started after it,
/// or it's older than `max_age_ms`. Consecutive same-speaker words
/// are grouped into phrases before committing.
#[allow(clippy::too_many_arguments)]
fn graduate_live_words(
    live_words: &Rc<RefCell<Vec<LiveWord>>>,
    transcript: &mut Signal<String>,
    last_speaker: &Rc<RefCell<String>>,
    turn_start1: f64,
    turn_start2: f64,
    session_start: f64,
    show_labels: bool,
    show_timestamps: bool,
) {
    let threshold = turn_start1.min(turn_start2);
    let max_age_ms = 15_000.0;
    let now_elapsed = js_sys::Date::now() - session_start;

    let mut words = live_words.borrow_mut();
    let mut graduated = 0;
    for word in words.iter() {
        let age = now_elapsed - word.estimated_ms;
        if word.estimated_ms < threshold || age > max_age_ms {
            graduated += 1;
        } else {
            break; // vec is sorted, so no more will qualify
        }
    }
    if graduated == 0 {
        return;
    }

    let cursor = get_cursor();
    let mut prev = transcript.peek().clone();
    let ls = last_speaker.borrow().clone();
    let mut last = ls;

    // Group consecutive same-speaker words into phrases
    let drained: Vec<LiveWord> = words.drain(..graduated).collect();
    let mut i = 0;
    while i < drained.len() {
        let speaker = &drained[i].speaker;
        let start_ms = drained[i].estimated_ms;
        let mut phrase = String::new();
        while i < drained.len() && drained[i].speaker == *speaker {
            if !phrase.is_empty() {
                phrase.push(' ');
            }
            phrase.push_str(&drained[i].word);
            i += 1;
        }

        let same_speaker = !last.is_empty() && last == *speaker;
        let mut line = String::new();
        if !same_speaker {
            if show_timestamps {
                line.push_str(&format_timestamp(start_ms));
                line.push(' ');
            }
            if show_labels {
                line.push_str(&format!("[{}] ", speaker));
            }
        }
        line.push_str(&phrase);
        let sep = if prev.is_empty() {
            ""
        } else if !same_speaker {
            "\n\n"
        } else {
            " "
        };
        prev = format!("{prev}{sep}{line}");
        last = speaker.clone();
    }

    *last_speaker.borrow_mut() = last;
    transcript.set(prev);
    if let Some((s, e)) = cursor {
        restore_cursor(s, e);
    }
}

/// Start capture for the given audio source.
async fn start_capture_for_source(
    source: &str,
    on_audio: impl Fn(Vec<f32>) + 'static,
) -> Result<BrowserCapture, wasm_bindgen::JsValue> {
    match source {
        "system" => BrowserCapture::start_system_audio(on_audio).await,
        _ => BrowserCapture::start(on_audio).await,
    }
}

// ── Root component ──────────────────────────────────────────────────
#[component]
pub fn App() -> Element {
    // ── Core signals ────────────────────────────────────────────────
    let mut rec_state = use_signal(|| RecState::Idle);
    let mut transcript = use_signal(String::new);
    let mut partial = use_signal(String::new);
    let mut partial2 = use_signal(String::new);
    let mut status_msg = use_signal(|| "Ready".to_string());
    let mut error_msg: Signal<Option<String>> = use_signal(|| None);
    // Settings-related signals were lifted to `Root` so the gear
    // button + drawer render at the shell level. We pull the same
    // `Copy` handles out of context so the rest of `App` can keep
    // referring to them by name. Spec: §6.4 mid-session voice change.
    let settings: crate::ui::app_state::AppSettings = use_context();
    let anthropic_configured = settings.anthropic_configured;
    let pipeline_stt_provider = settings.pipeline_stt_provider;
    let pipeline_tts_provider = settings.pipeline_tts_provider;
    let pipeline_tts_voice = settings.pipeline_tts_voice;
    let voice_credential = settings.voice_credential;
    let idle_minutes = settings.idle_minutes;
    let soniox_latency_mode = settings.soniox_latency_mode;
    let soniox_context_text = settings.soniox_context_text;
    let _ = (
        pipeline_tts_provider,
        pipeline_tts_voice,
        voice_credential,
        idle_minutes,
    );
    let mut countdown_secs: Signal<Option<u32>> = use_signal(|| None);
    let mut auto_scroll = use_signal(|| true);

    // ── Speaker settings (lifted to AppSettings/Root) ───────────────
    let speaker1_name = settings.speaker1_name;
    let speaker2_name = settings.speaker2_name;
    let speaker1_source = settings.speaker1_source;
    let speaker2_source = settings.speaker2_source;
    let speaker2_enabled = settings.speaker2_enabled;
    let show_labels = settings.show_labels;
    let show_timestamps = settings.show_timestamps;

    // ── Transfer / export ───────────────────────────────────────────
    let mut transfer_mode = use_signal(|| {
        load("parley_transfer_mode")
            .map(|s| TransferMode::from_cookie(&s))
            .unwrap_or(TransferMode::Copy)
    });
    let mut show_transfer_menu = use_signal(|| false);
    let mut prompt_filename = use_signal(|| {
        load("parley_prompt_filename")
            .map(|v| v == "true")
            .unwrap_or(false)
    });
    let mut transfer_feedback: Signal<Option<String>> = use_signal(|| None);
    let mut show_clear_confirm = use_signal(|| false);

    // ── Formatting settings (lifted to AppSettings/Root) ────────────
    // Spec `docs/global-reformat-spec.md` §3. The settings drawer is
    // the single source of truth for these now; the inline `▾`
    // format-combo dropdown was removed in favor of one global home.
    let reformat_model_config_id = settings.reformat_model_config_id;
    let reformat_credential = settings.reformat_credential;
    let auto_format_enabled = settings.auto_format_enabled;
    let format_nth = settings.format_nth;
    let format_depth = settings.format_depth;
    let format_context_depth = settings.format_context_depth;
    let format_on_stop = settings.format_on_stop;
    let reformatting = use_signal(|| false);

    // ── Cost tracking (show_cost_meter lifted to AppSettings/Root) ──
    let show_cost_meter = settings.show_cost_meter;
    // Accumulated STT cost in dollars (updated by ticker based on elapsed time)
    let mut stt_cost = use_signal(|| 0.0_f64);
    // Accumulated LLM cost in dollars (updated after each formatting call)
    let mut llm_cost = use_signal(|| 0.0_f64);

    // ── Speaker 1 handles ───────────────────────────────────────────
    let mut capture_handle: Signal<Option<Rc<RefCell<Option<BrowserCapture>>>>> =
        use_signal(|| None);
    let session_handle: Signal<Option<Rc<RefCell<Option<AssemblyAiSession>>>>> =
        use_signal(|| None);
    let mut soniox_session_handle: Signal<Option<Rc<RefCell<Option<SonioxSession>>>>> =
        use_signal(|| None);
    let mut current_turn_shared: Signal<Option<Rc<RefCell<String>>>> = use_signal(|| None);
    let mut current_turn_order_shared: Signal<Option<Rc<Cell<u32>>>> = use_signal(|| None);
    let mut turn_is_formatted1_shared: Signal<Option<Rc<Cell<bool>>>> = use_signal(|| None);

    // ── Speaker 2 handles ───────────────────────────────────────────
    let capture_handle2: Signal<Option<Rc<RefCell<Option<BrowserCapture>>>>> = use_signal(|| None);
    let session_handle2: Signal<Option<Rc<RefCell<Option<AssemblyAiSession>>>>> =
        use_signal(|| None);
    let mut current_turn_shared2: Signal<Option<Rc<RefCell<String>>>> = use_signal(|| None);
    let mut current_turn_order_shared2: Signal<Option<Rc<Cell<u32>>>> = use_signal(|| None);
    let mut turn_is_formatted2_shared: Signal<Option<Rc<Cell<bool>>>> = use_signal(|| None);

    // ── Live zone (multi-speaker chrono insertion) ───────────────────
    let mut live_turns_shared: Signal<Option<Rc<RefCell<Vec<LiveWord>>>>> = use_signal(|| None);
    let mut turn_start_time1_shared: Signal<Option<Rc<Cell<f64>>>> = use_signal(|| None);
    let mut turn_start_time2_shared: Signal<Option<Rc<Cell<f64>>>> = use_signal(|| None);
    // Reactive signal that mirrors live_turns length so UI re-renders
    let mut live_turns_version = use_signal(|| 0u32);

    // ── Shadow word graph ───────────────────────────────────────────
    // Per-word STT data lives here in parallel with the existing
    // string-based transcript pipeline. Nothing reads from it yet — this
    // is the migration path described in
    // `docs/conversation-mode-spec.md` §1.5.3 step 2. UI rendering and
    // the Haiku formatter remain unchanged for now; the graph is the
    // foundation that Conversation Mode will read from in a later phase.
    //
    // All ingest is `speaker = 0` until diarization wiring lands with
    // the multi-party work in Phase 8.
    let mut word_graph_shared: Signal<Option<Rc<RefCell<WordGraph>>>> = use_signal(|| None);

    // ── Shared recording state ──────────────────────────────────────
    let mut session_start_time: Signal<Option<f64>> = use_signal(|| None);
    let mut last_committed_speaker: Signal<Option<Rc<RefCell<String>>>> = use_signal(|| None);

    // Auto-scroll
    use_effect(move || {
        let _ = (transcript)();
        if (auto_scroll)() {
            scroll_textarea_to_bottom();
        }
    });

    use_effect(move || {
        if show_clear_confirm() {
            defer_ui_update(focus_clear_confirm_cancel);
        }
    });

    // ── Record (core logic) ─────────────────────────────────────────
    let mut start_recording = move || {
        // The AssemblyAI key now lives in the proxy's keystore;
        // we just hit /token and let the proxy return 412 if it's
        // not configured. The cookie-backed `api_key` signal is
        // legacy and will be removed in the upcoming Settings UI
        // pass.
        error_msg.set(None);
        status_msg.set("Connecting…".into());
        rec_state.set(RecState::Recording);

        let selected_stt_provider = pipeline_stt_provider.peek().clone();
        if selected_stt_provider == "xai" {
            error_msg.set(Some(
                "xAI STT is available in Conversation Mode only. Choose AssemblyAI or Soniox for Capture Mode.".into(),
            ));
            rec_state.set(RecState::Idle);
            status_msg.set("Ready".into());
            return;
        }

        if selected_stt_provider == "soniox" {
            let s1_source = speaker1_source.peek().clone();
            let latency_mode = soniox_latency_mode_from_value(&soniox_latency_mode.peek());
            let context_text = soniox_context_text.peek().clone();
            spawn_browser_task(async move {
                status_msg.set("Fetching Soniox token…".into());
                let token = match fetch_temp_api_key().await {
                    Ok(token) => token,
                    Err(err) => {
                        error_msg.set(Some(format!("Soniox token fetch failed: {err}")));
                        rec_state.set(RecState::Idle);
                        status_msg.set("Ready".into());
                        return;
                    }
                };

                status_msg.set("Connecting to Soniox…".into());
                let start_time = js_sys::Date::now();
                session_start_time.set(Some(start_time));
                last_committed_speaker.set(Some(Rc::new(RefCell::new(String::new()))));
                let graph_rc: Rc<RefCell<WordGraph>> = Rc::new(RefCell::new(WordGraph::new()));
                word_graph_shared.set(Some(graph_rc.clone()));

                let normalizer = Rc::new(RefCell::new(TokenStreamNormalizer::new()));
                let provisional_by_lane: Rc<RefCell<BTreeMap<u8, String>>> =
                    Rc::new(RefCell::new(BTreeMap::new()));
                let last_lane: Rc<RefCell<Option<u8>>> = Rc::new(RefCell::new(None));
                // Soniox often finalizes text continuously without emitting a
                // provider turn boundary. Treat each non-empty finalized batch
                // as a formatter-countable commit so live formatting cannot
                // starve while a long utterance is still in progress.
                let soniox_sentence_counter: Rc<Cell<u32>> = Rc::new(Cell::new(0));
                let soniox_pending_chars: Rc<Cell<usize>> = Rc::new(Cell::new(0));

                let session = match SonioxSession::connect(
                    &token,
                    SonioxConfig::for_latency_mode_and_context(latency_mode, Some(context_text)),
                    {
                        let normalizer = normalizer.clone();
                        let provisional_by_lane = provisional_by_lane.clone();
                        let graph_rc = graph_rc.clone();
                        let last_lane = last_lane.clone();
                        let sentence_counter = soniox_sentence_counter.clone();
                        let pending_chars = soniox_pending_chars.clone();
                        move |event: SttStreamEvent| {
                            if let SttStreamEvent::Error { message, .. } = &event {
                                let message = message.clone();
                                defer_ui_update(move || {
                                    error_msg.set(Some(format!("Soniox error: {message}")));
                                    rec_state.set(RecState::Idle);
                                    status_msg.set("Ready".into());
                                });
                                return;
                            }

                            let batch = match normalizer.borrow_mut().accept_event(event) {
                                Ok(batch) => batch,
                                Err(err) => {
                                    defer_ui_update(move || {
                                        error_msg.set(Some(format!(
                                            "Soniox normalization failed: {err}"
                                        )));
                                        rec_state.set(RecState::Idle);
                                        status_msg.set("Ready".into());
                                    });
                                    return;
                                }
                            };

                            batch.apply_to_graph(&mut graph_rc.borrow_mut());
                            let provisional_by_lane = provisional_by_lane.clone();
                            let last_lane = last_lane.clone();
                            let sentence_counter = sentence_counter.clone();
                            let pending_chars = pending_chars.clone();
                            defer_ui_update(move || {
                                let mut committed_finalized_text = false;
                                let mut committed_text_for_format = String::new();
                                let speaker1 = speaker1_name.peek().clone();
                                let speaker2 = speaker2_name.peek().clone();
                                let show_labels_now = *show_labels.peek();
                                let show_timestamps_now = *show_timestamps.peek();
                                for update in &batch.updates {
                                    let finalized_text = render_stt_words(&update.finalized);
                                    if !finalized_text.is_empty() {
                                        committed_finalized_text = true;
                                        if !committed_text_for_format.is_empty() {
                                            committed_text_for_format.push(' ');
                                        }
                                        committed_text_for_format.push_str(finalized_text.trim());
                                        let lane_name =
                                            soniox_lane_name(update.lane, &speaker1, &speaker2);
                                        let start_ms = update
                                            .finalized
                                            .first()
                                            .map(|word| word.start_ms)
                                            .unwrap_or_else(|| js_sys::Date::now() - start_time);
                                        transcript.with_mut(|body| {
                                            if !body.is_empty() {
                                                body.push(' ');
                                            }
                                            let speaker_changed = last_lane
                                                .borrow()
                                                .map(|lane| lane != update.lane)
                                                .unwrap_or(true);
                                            if show_timestamps_now && speaker_changed {
                                                body.push_str(&format_timestamp(start_ms));
                                                body.push(' ');
                                            }
                                            if show_labels_now && speaker_changed {
                                                body.push('[');
                                                body.push_str(&lane_name);
                                                body.push_str("] ");
                                            }
                                            body.push_str(finalized_text.trim());
                                        });
                                        *last_lane.borrow_mut() = Some(update.lane);
                                    }

                                    let provisional_text = render_stt_words(&update.provisional);
                                    if provisional_text.is_empty() {
                                        provisional_by_lane.borrow_mut().remove(&update.lane);
                                    } else {
                                        provisional_by_lane
                                            .borrow_mut()
                                            .insert(update.lane, provisional_text);
                                    }
                                }

                                let provisional = provisional_by_lane
                                    .borrow()
                                    .iter()
                                    .map(|(lane, text)| {
                                        let lane_name =
                                            soniox_lane_name(*lane, &speaker1, &speaker2);
                                        if show_labels_now {
                                            format!("[{lane_name}] {text}")
                                        } else {
                                            text.clone()
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .join(" ");
                                partial.set(provisional);
                                partial2.set(String::new());

                                if committed_finalized_text {
                                    note_committed_text_and_maybe_format(
                                        &sentence_counter,
                                        &pending_chars,
                                        &committed_text_for_format,
                                        anthropic_configured,
                                        auto_format_enabled,
                                        format_nth,
                                        reformat_model_config_id,
                                        reformat_credential,
                                        format_depth,
                                        format_context_depth,
                                        transcript,
                                        llm_cost,
                                        false,
                                    );
                                }

                                if batch.has_turn_boundary() {
                                    provisional_by_lane.borrow_mut().clear();
                                    partial.set(String::new());
                                }
                            });
                        }
                    },
                    move |_code, _reason| {},
                ) {
                    Ok(session) => session,
                    Err(err) => {
                        error_msg.set(Some(format!("Soniox WebSocket failed: {err:?}")));
                        rec_state.set(RecState::Idle);
                        status_msg.set("Ready".into());
                        return;
                    }
                };

                let session_rc: Rc<RefCell<Option<SonioxSession>>> =
                    Rc::new(RefCell::new(Some(session)));
                soniox_session_handle.set(Some(session_rc.clone()));
                let session_for_audio = session_rc.clone();

                match start_capture_for_source(&s1_source, move |samples: Vec<f32>| {
                    if let Some(ref session) = *session_for_audio.borrow() {
                        let _ = session.send_audio(&samples);
                    }
                })
                .await
                {
                    Ok(capture) => {
                        capture_handle.set(Some(Rc::new(RefCell::new(Some(capture)))));
                        status_msg.set("Recording with Soniox…".into());
                    }
                    Err(err) => {
                        error_msg.set(Some(format!("Mic access denied: {err:?}")));
                        if let Some(session) = session_rc.borrow_mut().take() {
                            let _ = session.finish();
                        }
                        rec_state.set(RecState::Idle);
                        status_msg.set("Ready".into());
                    }
                }
            });
            return;
        }

        let multi = *speaker2_enabled.peek();
        let s1_source = speaker1_source.peek().clone();
        let s2_source = speaker2_source.peek().clone();

        spawn_browser_task(async move {
            // Fetch token for speaker 1
            status_msg.set("Fetching token…".into());
            let token1 = match fetch_temp_token().await {
                Ok(t) => t,
                Err(e) => {
                    error_msg.set(Some(format!("Token fetch failed: {e}")));
                    rec_state.set(RecState::Idle);
                    status_msg.set("Ready".into());
                    return;
                }
            };

            // Fetch token for speaker 2 if multi-speaker
            let token2 = if multi {
                match fetch_temp_token().await {
                    Ok(t) => Some(t),
                    Err(e) => {
                        error_msg.set(Some(format!("Token fetch (speaker 2) failed: {e}")));
                        rec_state.set(RecState::Idle);
                        status_msg.set("Ready".into());
                        return;
                    }
                }
            } else {
                None
            };

            status_msg.set("Connecting…".into());

            // Shared state
            let last_activity = Rc::new(Cell::new(js_sys::Date::now()));
            let start_time = js_sys::Date::now();
            session_start_time.set(Some(start_time));
            let last_speaker: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
            last_committed_speaker.set(Some(last_speaker.clone()));

            // Live zone state (only meaningful in multi mode, but always created)
            let live_turns_rc: Rc<RefCell<Vec<LiveWord>>> = Rc::new(RefCell::new(Vec::new()));
            live_turns_shared.set(Some(live_turns_rc.clone()));
            let turn_start1: Rc<Cell<f64>> = Rc::new(Cell::new(f64::MAX));
            let turn_start2: Rc<Cell<f64>> = Rc::new(Cell::new(f64::MAX));
            turn_start_time1_shared.set(Some(turn_start1.clone()));
            turn_start_time2_shared.set(Some(turn_start2.clone()));

            // Shadow word graph — fresh per session. Both s1 and s2 callbacks
            // ingest into it as `speaker = 0` (single-lane until Phase 8).
            let graph_rc: Rc<RefCell<WordGraph>> = Rc::new(RefCell::new(WordGraph::new()));
            word_graph_shared.set(Some(graph_rc.clone()));

            // ── Speaker 1 session ───────────────────────────────────
            let session1_rc: Rc<RefCell<Option<AssemblyAiSession>>> = Rc::new(RefCell::new(None));
            // Session 2 ref for cross-session force endpoint
            let session2_rc_for_s1: Rc<RefCell<Option<AssemblyAiSession>>> =
                Rc::new(RefCell::new(None));

            let current_turn1: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
            let current_turn_order1: Rc<Cell<u32>> = Rc::new(Cell::new(u32::MAX));
            let format_sentence_counter: Rc<Cell<u32>> = Rc::new(Cell::new(0));
            let format_pending_chars: Rc<Cell<usize>> = Rc::new(Cell::new(0));
            // Track when the current turn started speaking
            let speech_start1: Rc<Cell<f64>> = Rc::new(Cell::new(0.0));
            let formatted_flag1: Rc<Cell<bool>> = Rc::new(Cell::new(false));

            current_turn_shared.set(Some(current_turn1.clone()));
            current_turn_order_shared.set(Some(current_turn_order1.clone()));
            turn_is_formatted1_shared.set(Some(formatted_flag1.clone()));

            let session1 = {
                let last_activity = last_activity.clone();
                let last_speaker = last_speaker.clone();
                let ct = current_turn1.clone();
                let cto = current_turn_order1.clone();
                let sentence_counter = format_sentence_counter.clone();
                let pending_chars = format_pending_chars.clone();
                let t_sig = transcript;
                let p_sig = partial;
                let ss1 = speech_start1.clone();
                let ts1 = turn_start1.clone();
                let live_rc = live_turns_rc.clone();
                let s2_for_force = session2_rc_for_s1.clone();
                let ltv = live_turns_version;
                let ff1 = formatted_flag1.clone();
                let graph_for_s1 = graph_rc.clone();

                match AssemblyAiSession::connect(
                    &token1,
                    move |event: TurnEvent| {
                        // Ingest per-word data into the shadow graph BEFORE
                        // any string-based UI work. Speaker = 0; in
                        // multi-speaker mode s2 ingests into speaker = 1
                        // (perfect diarization by physical channel).
                        if !event.words.is_empty() {
                            graph_for_s1.borrow_mut().ingest_turn(
                                0,
                                &event.words,
                                event.end_of_turn,
                            );
                        }

                        let TurnEvent {
                            transcript: text,
                            is_formatted,
                            turn_order,
                            end_of_turn,
                            ..
                        } = event;
                        last_activity.set(js_sys::Date::now());
                        let text = text.replace('\n', " ").replace("  ", " ");
                        let ct = ct.clone();
                        let cto = cto.clone();
                        let sentence_counter = sentence_counter.clone();
                        let pending_chars = pending_chars.clone();
                        let last_speaker = last_speaker.clone();
                        let ss1 = ss1.clone();
                        let ts1 = ts1.clone();
                        let live_rc = live_rc.clone();
                        let s2_for_force = s2_for_force.clone();
                        let ff1 = ff1.clone();
                        let mut t_sig = t_sig;
                        let mut p_sig = p_sig;
                        let mut ltv = ltv;
                        defer_ui_update(move || {
                            let prev_order = cto.get();
                            let is_new_turn = turn_order != prev_order;

                            if is_new_turn && prev_order != u32::MAX {
                                let old_turn = ct.borrow().clone();
                                if !old_turn.is_empty() {
                                    let name = speaker1_name.peek().clone();
                                    if multi {
                                        // Cross-session force endpoint
                                        if let Some(ref sess) = *s2_for_force.borrow() {
                                            let _ = sess.force_endpoint();
                                        }
                                        // Insert words into live zone at chrono positions
                                        let elapsed = ss1.get();
                                        let new_words =
                                            split_turn_to_words(elapsed, &name, &old_turn);
                                        let mut words = live_rc.borrow_mut();
                                        for w in new_words {
                                            let pos = words.partition_point(|x| {
                                                x.estimated_ms <= w.estimated_ms
                                            });
                                            words.insert(pos, w);
                                        }
                                        drop(words);
                                        let previous_live_turns_version = *ltv.peek();
                                        ltv.set(previous_live_turns_version + 1);
                                    } else {
                                        // Single speaker: direct to transcript
                                        let cursor = get_cursor();
                                        let prev = t_sig.peek().clone();
                                        let elapsed = js_sys::Date::now() - start_time;
                                        let new_text = build_committed_text(
                                            &prev,
                                            &old_turn,
                                            &name,
                                            false,
                                            *show_labels.peek(),
                                            *show_timestamps.peek(),
                                            elapsed,
                                            &last_speaker.borrow(),
                                        );
                                        t_sig.set(new_text);
                                        *last_speaker.borrow_mut() = name;
                                        if let Some((s, e)) = cursor {
                                            restore_cursor(s, e);
                                        }
                                        note_committed_text_and_maybe_format(
                                            &sentence_counter,
                                            &pending_chars,
                                            &old_turn,
                                            anthropic_configured,
                                            auto_format_enabled,
                                            format_nth,
                                            reformat_model_config_id,
                                            reformat_credential,
                                            format_depth,
                                            format_context_depth,
                                            t_sig,
                                            llm_cost,
                                            false,
                                        );
                                    }
                                }
                            }

                            if is_new_turn {
                                // Record when this new turn started speaking
                                let elapsed = js_sys::Date::now() - start_time;
                                ss1.set(elapsed);
                                ts1.set(elapsed);
                            }

                            if end_of_turn {
                                let final_turn = if text.trim().is_empty() {
                                    ct.borrow().clone()
                                } else {
                                    text.clone()
                                };
                                if !final_turn.is_empty() {
                                    let name = speaker1_name.peek().clone();
                                    if multi {
                                        if let Some(ref sess) = *s2_for_force.borrow() {
                                            let _ = sess.force_endpoint();
                                        }
                                        let elapsed = ss1.get();
                                        let new_words =
                                            split_turn_to_words(elapsed, &name, &final_turn);
                                        let mut words = live_rc.borrow_mut();
                                        for w in new_words {
                                            let pos = words.partition_point(|x| {
                                                x.estimated_ms <= w.estimated_ms
                                            });
                                            words.insert(pos, w);
                                        }
                                        drop(words);
                                        let previous_live_turns_version = *ltv.peek();
                                        ltv.set(previous_live_turns_version + 1);
                                    } else {
                                        let cursor = get_cursor();
                                        let prev = t_sig.peek().clone();
                                        let elapsed = js_sys::Date::now() - start_time;
                                        let new_text = build_committed_text(
                                            &prev,
                                            &final_turn,
                                            &name,
                                            false,
                                            *show_labels.peek(),
                                            *show_timestamps.peek(),
                                            elapsed,
                                            &last_speaker.borrow(),
                                        );
                                        t_sig.set(new_text);
                                        *last_speaker.borrow_mut() = name;
                                        if let Some((s, e)) = cursor {
                                            restore_cursor(s, e);
                                        }
                                        note_committed_text_and_maybe_format(
                                            &sentence_counter,
                                            &pending_chars,
                                            &final_turn,
                                            anthropic_configured,
                                            auto_format_enabled,
                                            format_nth,
                                            reformat_model_config_id,
                                            reformat_credential,
                                            format_depth,
                                            format_context_depth,
                                            t_sig,
                                            llm_cost,
                                            false,
                                        );
                                    }
                                }
                                cto.set(u32::MAX);
                                ct.borrow_mut().clear();
                                p_sig.set(String::new());
                                ff1.set(is_formatted);
                                return;
                            }

                            cto.set(turn_order);
                            *ct.borrow_mut() = text.clone();
                            p_sig.set(text);
                            ff1.set(is_formatted);
                        });
                    },
                    {
                        let mut rec_state = rec_state;
                        let mut status_msg = status_msg;
                        let mut error_msg = error_msg;
                        move |code: u16, reason: String| {
                            defer_ui_update(move || {
                                rec_state.set(RecState::Stopped);
                                if code == 4001 {
                                    error_msg
                                        .set(Some("Not authorized — check your API key".into()));
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
                                    error_msg.set(Some(format!(
                                        "Connection closed (code {code}): {reason}"
                                    )));
                                    status_msg.set("Disconnected".into());
                                } else {
                                    status_msg.set("Disconnected".into());
                                }
                            });
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
                }
            };

            *session1_rc.borrow_mut() = Some(session1);
            session_handle.clone().set(Some(session1_rc.clone()));

            // Start audio capture for speaker 1
            let session1_rc2 = session1_rc.clone();
            match start_capture_for_source(&s1_source, move |samples| {
                if let Some(ref sess) = *session1_rc2.borrow() {
                    let _ = sess.send_audio(&samples);
                }
            })
            .await
            {
                Ok(cap) => {
                    let cap_rc: Rc<RefCell<Option<BrowserCapture>>> =
                        Rc::new(RefCell::new(Some(cap)));
                    capture_handle.clone().set(Some(cap_rc.clone()));

                    // ── Speaker 2 session (if multi-speaker) ────────
                    let cap2_rc: Option<Rc<RefCell<Option<BrowserCapture>>>> =
                        if let Some(ref tok2) = token2 {
                            // Use session2_rc_for_s1 as the actual session2 holder
                            // (speaker 1's callback already has a reference to it)
                            let session2_rc = session2_rc_for_s1.clone();

                            let ct2: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
                            let cto2: Rc<Cell<u32>> = Rc::new(Cell::new(u32::MAX));
                            let speech_start2: Rc<Cell<f64>> = Rc::new(Cell::new(0.0));
                            let formatted_flag2: Rc<Cell<bool>> = Rc::new(Cell::new(false));

                            current_turn_shared2.set(Some(ct2.clone()));
                            current_turn_order_shared2.set(Some(cto2.clone()));
                            turn_is_formatted2_shared.set(Some(formatted_flag2.clone()));

                            let session2 = {
                                let last_activity = last_activity.clone();
                                let ct = ct2.clone();
                                let cto = cto2.clone();
                                let p2_sig = partial2;
                                let ss2 = speech_start2.clone();
                                let ts2 = turn_start2.clone();
                                let live_rc = live_turns_rc.clone();
                                let s1_for_force = session1_rc.clone();
                                let ltv = live_turns_version;
                                let ff2 = formatted_flag2.clone();
                                let graph_for_s2 = graph_rc.clone();

                                match AssemblyAiSession::connect(
                                    tok2,
                                    move |event: TurnEvent| {
                                        // Ingest per-word data into the
                                        // shadow graph. Speaker = 1 because
                                        // s2 = the second physical mic;
                                        // diarization-by-channel is exact.
                                        // Phase 8 adds AI-diarization for
                                        // the single-mic / multiple-voices
                                        // case via `speaker_label`.
                                        if !event.words.is_empty() {
                                            graph_for_s2.borrow_mut().ingest_turn(
                                                1,
                                                &event.words,
                                                event.end_of_turn,
                                            );
                                        }

                                        let TurnEvent {
                                            transcript: text,
                                            is_formatted,
                                            turn_order,
                                            end_of_turn,
                                            ..
                                        } = event;
                                        last_activity.set(js_sys::Date::now());
                                        let text = text.replace('\n', " ").replace("  ", " ");
                                        let ct = ct.clone();
                                        let cto = cto.clone();
                                        let ss2 = ss2.clone();
                                        let ts2 = ts2.clone();
                                        let live_rc = live_rc.clone();
                                        let s1_for_force = s1_for_force.clone();
                                        let ff2 = ff2.clone();
                                        let mut p2_sig = p2_sig;
                                        let mut ltv = ltv;
                                        defer_ui_update(move || {
                                            let prev_order = cto.get();
                                            let is_new_turn = turn_order != prev_order;

                                            if is_new_turn && prev_order != u32::MAX {
                                                let old_turn = ct.borrow().clone();
                                                if !old_turn.is_empty() {
                                                    let name = speaker2_name.peek().clone();
                                                    // Cross-session force endpoint
                                                    if let Some(ref sess) = *s1_for_force.borrow() {
                                                        let _ = sess.force_endpoint();
                                                    }
                                                    // Insert words into live zone at chrono positions
                                                    let elapsed = ss2.get();
                                                    let new_words = split_turn_to_words(
                                                        elapsed, &name, &old_turn,
                                                    );
                                                    let mut words = live_rc.borrow_mut();
                                                    for w in new_words {
                                                        let pos = words.partition_point(|x| {
                                                            x.estimated_ms <= w.estimated_ms
                                                        });
                                                        words.insert(pos, w);
                                                    }
                                                    drop(words);
                                                    let previous_live_turns_version = *ltv.peek();
                                                    ltv.set(previous_live_turns_version + 1);
                                                }
                                            }

                                            if is_new_turn {
                                                let elapsed = js_sys::Date::now() - start_time;
                                                ss2.set(elapsed);
                                                ts2.set(elapsed);
                                            }

                                            if end_of_turn {
                                                let final_turn = if text.trim().is_empty() {
                                                    ct.borrow().clone()
                                                } else {
                                                    text.clone()
                                                };
                                                if !final_turn.is_empty() {
                                                    let name = speaker2_name.peek().clone();
                                                    if let Some(ref sess) = *s1_for_force.borrow() {
                                                        let _ = sess.force_endpoint();
                                                    }
                                                    let elapsed = ss2.get();
                                                    let new_words = split_turn_to_words(
                                                        elapsed,
                                                        &name,
                                                        &final_turn,
                                                    );
                                                    let mut words = live_rc.borrow_mut();
                                                    for w in new_words {
                                                        let pos = words.partition_point(|x| {
                                                            x.estimated_ms <= w.estimated_ms
                                                        });
                                                        words.insert(pos, w);
                                                    }
                                                    drop(words);
                                                    let previous_live_turns_version = *ltv.peek();
                                                    ltv.set(previous_live_turns_version + 1);
                                                }
                                                cto.set(u32::MAX);
                                                ct.borrow_mut().clear();
                                                p2_sig.set(String::new());
                                                ff2.set(is_formatted);
                                                return;
                                            }

                                            cto.set(turn_order);
                                            *ct.borrow_mut() = text.clone();
                                            p2_sig.set(text);
                                            ff2.set(is_formatted);
                                        });
                                    },
                                    {
                                        let mut rec_state = rec_state;
                                        let mut status_msg = status_msg;
                                        let mut error_msg = error_msg;
                                        move |code: u16, reason: String| {
                                            defer_ui_update(move || {
                                                rec_state.set(RecState::Stopped);
                                                if code != 1000 {
                                                    error_msg.set(Some(format!(
                                                    "Speaker 2 disconnected (code {code}): {reason}"
                                                )));
                                                }
                                                status_msg.set("Disconnected".into());
                                            });
                                        }
                                    },
                                ) {
                                    Ok(s) => s,
                                    Err(e) => {
                                        error_msg.set(Some(format!(
                                            "Speaker 2 WS connect failed: {e:?}"
                                        )));
                                        rec_state.set(RecState::Idle);
                                        status_msg.set("Ready".into());
                                        // Clean up speaker 1
                                        if let Some(cap) = cap_rc.borrow_mut().take() {
                                            cap.stop();
                                        }
                                        if let Some(ref sess) = *session1_rc.borrow() {
                                            let _ = sess.terminate();
                                        }
                                        return;
                                    }
                                }
                            };

                            *session2_rc.borrow_mut() = Some(session2);
                            session_handle2.clone().set(Some(session2_rc.clone()));

                            // Start audio capture for speaker 2
                            let session2_rc2 = session2_rc.clone();
                            match start_capture_for_source(&s2_source, move |samples| {
                                if let Some(ref sess) = *session2_rc2.borrow() {
                                    let _ = sess.send_audio(&samples);
                                }
                            })
                            .await
                            {
                                Ok(cap2) => {
                                    let c2rc: Rc<RefCell<Option<BrowserCapture>>> =
                                        Rc::new(RefCell::new(Some(cap2)));
                                    capture_handle2.clone().set(Some(c2rc.clone()));
                                    Some(c2rc)
                                }
                                Err(e) => {
                                    error_msg
                                        .set(Some(format!("System audio capture failed: {e:?}")));
                                    rec_state.set(RecState::Idle);
                                    status_msg.set("Ready".into());
                                    if let Some(cap) = cap_rc.borrow_mut().take() {
                                        cap.stop();
                                    }
                                    if let Some(ref sess) = *session1_rc.borrow() {
                                        let _ = sess.terminate();
                                    }
                                    if let Some(ref sess) = *session2_rc.borrow() {
                                        let _ = sess.terminate();
                                    }
                                    return;
                                }
                            }
                        } else {
                            None
                        };

                    status_msg.set("Recording…".into());

                    // ── Countdown ticker ─────────────────────────────
                    countdown_secs.set(Some(*idle_minutes.peek() * 60));
                    let last_activity = last_activity.clone();
                    let session_for_timeout = session1_rc.clone();
                    let cap_for_timeout = cap_rc;
                    let session2_for_timeout = if multi {
                        session_handle2.peek().clone()
                    } else {
                        None
                    };
                    let cap2_for_timeout = cap2_rc;
                    let mut rec_state = rec_state;
                    let mut status_msg = status_msg;
                    let mut partial_t = partial;
                    let mut partial2_t = partial2;
                    let mut transcript_t = transcript;
                    let mut countdown_secs = countdown_secs;
                    let ticker_last_speaker = last_speaker.clone();
                    let ticker_live_turns = live_turns_rc.clone();
                    let ticker_ts1 = turn_start1.clone();
                    let ticker_ts2 = turn_start2.clone();
                    let ticker_sentence_counter = format_sentence_counter.clone();
                    let ticker_pending_chars = format_pending_chars.clone();
                    let mut ticker_ltv = live_turns_version;

                    spawn_browser_task(async move {
                        let mut blink_on = false;
                        let mut beep_cooldown: u32 = 0;
                        loop {
                            gloo_timers::future::TimeoutFuture::new(1_000).await;
                            if *rec_state.peek() != RecState::Recording {
                                countdown_secs.set(None);
                                set_tab_title("Parley");
                                break;
                            }
                            let timeout_total_secs = *idle_minutes.peek() * 60;
                            let elapsed_ms = js_sys::Date::now() - last_activity.get();
                            let timeout_ms = (timeout_total_secs as f64) * 1000.0;
                            let remaining_ms = (timeout_ms - elapsed_ms).max(0.0);
                            let remaining_secs = (remaining_ms / 1000.0).ceil() as u32;
                            countdown_secs.set(Some(remaining_secs));

                            // STT cost accumulation (runs every second while recording)
                            {
                                // $0.45/hr per session = $0.000125/sec
                                let rate_per_sec = if multi { 0.000125 * 2.0 } else { 0.000125 };
                                let previous_stt_cost = *stt_cost.peek();
                                stt_cost.set(previous_stt_cost + rate_per_sec);
                            }

                            // Graduate live turns (multi-speaker only)
                            if multi {
                                let transcript_before_graduation = transcript_t.peek().clone();
                                let before = ticker_live_turns.borrow().len();
                                graduate_live_words(
                                    &ticker_live_turns,
                                    &mut transcript_t,
                                    &ticker_last_speaker,
                                    ticker_ts1.get(),
                                    ticker_ts2.get(),
                                    start_time,
                                    *show_labels.peek(),
                                    *show_timestamps.peek(),
                                );
                                let after = ticker_live_turns.borrow().len();
                                if before != after {
                                    let previous_live_turns_version = *ticker_ltv.peek();
                                    ticker_ltv.set(previous_live_turns_version + 1);
                                    let transcript_after_graduation = transcript_t.peek().clone();
                                    let committed_text = transcript_after_graduation
                                        .strip_prefix(&transcript_before_graduation)
                                        .unwrap_or(&transcript_after_graduation);
                                    note_committed_text_and_maybe_format(
                                        &ticker_sentence_counter,
                                        &ticker_pending_chars,
                                        committed_text,
                                        anthropic_configured,
                                        auto_format_enabled,
                                        format_nth,
                                        reformat_model_config_id,
                                        reformat_credential,
                                        format_depth,
                                        format_context_depth,
                                        transcript_t,
                                        llm_cost,
                                        true,
                                    );
                                }
                            }

                            // 1-minute warning
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
                                set_tab_title("Parley");
                                // Stop speaker 1
                                if let Some(cap) = cap_for_timeout.borrow_mut().take() {
                                    cap.stop();
                                }
                                if let Some(ref sess) = *session_for_timeout.borrow() {
                                    let _ = sess.terminate();
                                }
                                // Stop speaker 2
                                if let Some(ref c2rc) = cap2_for_timeout
                                    && let Some(cap2) = c2rc.borrow_mut().take()
                                {
                                    cap2.stop();
                                }
                                if let Some(ref s2rc) = session2_for_timeout
                                    && let Some(ref sess) = *s2rc.borrow()
                                {
                                    let _ = sess.terminate();
                                }
                                // Flush partials
                                let p1 = partial_t.peek().clone();
                                if !p1.is_empty() {
                                    if multi {
                                        let elapsed = js_sys::Date::now() - start_time;
                                        let new_words = split_turn_to_words(
                                            elapsed,
                                            &speaker1_name.peek(),
                                            &p1,
                                        );
                                        let mut words = ticker_live_turns.borrow_mut();
                                        for w in new_words {
                                            let pos = words.partition_point(|x| {
                                                x.estimated_ms <= w.estimated_ms
                                            });
                                            words.insert(pos, w);
                                        }
                                    } else {
                                        let prev = transcript_t.peek().clone();
                                        let name = speaker1_name.peek().clone();
                                        let elapsed = js_sys::Date::now() - start_time;
                                        let new_text = build_committed_text(
                                            &prev,
                                            &p1,
                                            &name,
                                            false,
                                            *show_labels.peek(),
                                            *show_timestamps.peek(),
                                            elapsed,
                                            &ticker_last_speaker.borrow(),
                                        );
                                        transcript_t.set(new_text);
                                        *ticker_last_speaker.borrow_mut() = name;
                                    }
                                    partial_t.set(String::new());
                                }
                                if multi {
                                    let p2 = partial2_t.peek().clone();
                                    if !p2.is_empty() {
                                        let elapsed = js_sys::Date::now() - start_time;
                                        let new_words = split_turn_to_words(
                                            elapsed,
                                            &speaker2_name.peek(),
                                            &p2,
                                        );
                                        let mut words = ticker_live_turns.borrow_mut();
                                        for w in new_words {
                                            let pos = words.partition_point(|x| {
                                                x.estimated_ms <= w.estimated_ms
                                            });
                                            words.insert(pos, w);
                                        }
                                        drop(words);
                                        partial2_t.set(String::new());
                                    }
                                    // Force-graduate all remaining live words
                                    ticker_ts1.set(0.0);
                                    ticker_ts2.set(0.0);
                                    graduate_live_words(
                                        &ticker_live_turns,
                                        &mut transcript_t,
                                        &ticker_last_speaker,
                                        f64::MAX,
                                        f64::MAX,
                                        start_time,
                                        *show_labels.peek(),
                                        *show_timestamps.peek(),
                                    );
                                    let previous_live_turns_version = *ticker_ltv.peek();
                                    ticker_ltv.set(previous_live_turns_version + 1);
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
                    if let Some(ref sess) = *session1_rc.borrow() {
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

    // ── End Turn (speaker 1) ────────────────────────────────────────
    let on_end_turn1 = move |_| {
        if pipeline_stt_provider.peek().as_str() == "soniox" {
            if let Some(sess_rc) = soniox_session_handle.peek().as_ref() {
                let latency_mode = soniox_latency_mode_from_value(&soniox_latency_mode.peek());
                finalize_soniox_session_after_settle(sess_rc.clone(), latency_mode);
            }
            return;
        }

        if let Some(sess_rc) = session_handle.peek().as_ref()
            && let Some(ref sess) = *sess_rc.borrow()
        {
            let _ = sess.force_endpoint();
        }
        let p = partial.peek().clone();
        if !p.is_empty() {
            let multi = *speaker2_enabled.peek();
            let name = speaker1_name.peek().clone();
            let start = session_start_time.peek().unwrap_or(0.0);
            if multi {
                if let Some(ref live_rc) = *live_turns_shared.peek() {
                    let elapsed = js_sys::Date::now() - start;
                    let new_words = split_turn_to_words(elapsed, &name, &p);
                    let mut words = live_rc.borrow_mut();
                    for w in new_words {
                        let pos = words.partition_point(|x| x.estimated_ms <= w.estimated_ms);
                        words.insert(pos, w);
                    }
                    drop(words);
                    let previous_live_turns_version = *live_turns_version.peek();
                    live_turns_version.set(previous_live_turns_version + 1);
                }
            } else {
                let prev = transcript.peek().clone();
                let ls = last_committed_speaker.peek().clone();
                let ls_name = ls.as_ref().map(|r| r.borrow().clone()).unwrap_or_default();
                let elapsed = js_sys::Date::now() - start;
                let new_text = build_committed_text(
                    &prev,
                    &p,
                    &name,
                    false,
                    *show_labels.peek(),
                    *show_timestamps.peek(),
                    elapsed,
                    &ls_name,
                );
                transcript.set(new_text);
                if let Some(ref ls_rc) = ls {
                    *ls_rc.borrow_mut() = name;
                }
            }
            partial.set(String::new());
        }
        if let Some(ct) = current_turn_shared.peek().as_ref() {
            *ct.borrow_mut() = String::new();
        }
        if let Some(cto) = current_turn_order_shared.peek().as_ref() {
            cto.set(u32::MAX);
        }
    };

    // ── End Turn (speaker 2) ────────────────────────────────────────
    let on_end_turn2 = move |_| {
        if pipeline_stt_provider.peek().as_str() == "soniox" {
            if let Some(sess_rc) = soniox_session_handle.peek().as_ref() {
                let latency_mode = soniox_latency_mode_from_value(&soniox_latency_mode.peek());
                finalize_soniox_session_after_settle(sess_rc.clone(), latency_mode);
            }
            return;
        }

        if let Some(sess_rc) = session_handle2.peek().as_ref()
            && let Some(ref sess) = *sess_rc.borrow()
        {
            let _ = sess.force_endpoint();
        }
        let p = partial2.peek().clone();
        if !p.is_empty() {
            let name = speaker2_name.peek().clone();
            let start = session_start_time.peek().unwrap_or(0.0);
            if let Some(ref live_rc) = *live_turns_shared.peek() {
                let elapsed = js_sys::Date::now() - start;
                let new_words = split_turn_to_words(elapsed, &name, &p);
                let mut words = live_rc.borrow_mut();
                for w in new_words {
                    let pos = words.partition_point(|x| x.estimated_ms <= w.estimated_ms);
                    words.insert(pos, w);
                }
                drop(words);
                let previous_live_turns_version = *live_turns_version.peek();
                live_turns_version.set(previous_live_turns_version + 1);
            }
            partial2.set(String::new());
        }
        if let Some(ct) = current_turn_shared2.peek().as_ref() {
            *ct.borrow_mut() = String::new();
        }
        if let Some(cto) = current_turn_order_shared2.peek().as_ref() {
            cto.set(u32::MAX);
        }
    };

    // ── Stop ────────────────────────────────────────────────────────
    let on_stop = move |_| {
        // Immediately enter Stopping state — disables all buttons
        rec_state.set(RecState::Stopping);
        status_msg.set("Waiting for final transcript\u{2026}".into());

        if pipeline_stt_provider.peek().as_str() == "soniox" {
            spawn_browser_task(async move {
                if let Some(cap_rc) = capture_handle.peek().as_ref()
                    && let Some(capture) = cap_rc.borrow_mut().take()
                {
                    capture.stop();
                }

                if let Some(sess_rc) = soniox_session_handle.peek().as_ref()
                    && let Some(session) = sess_rc.borrow_mut().take()
                {
                    let _ = session.finish();
                }

                gloo_timers::future::TimeoutFuture::new(800).await;

                partial.set(String::new());
                partial2.set(String::new());
                rec_state.set(RecState::Stopped);
                status_msg.set("Stopped".into());
            });
            return;
        }

        spawn_browser_task(async move {
            let multi = *speaker2_enabled.peek();

            // 1. Stop audio capture — no more audio sent to STT
            if let Some(cap_rc) = capture_handle.peek().as_ref()
                && let Some(cap) = cap_rc.borrow_mut().take()
            {
                cap.stop();
            }
            if multi
                && let Some(cap_rc) = capture_handle2.peek().as_ref()
                && let Some(cap) = cap_rc.borrow_mut().take()
            {
                cap.stop();
            }

            // 2. Force endpoint on active sessions and reset formatted flags
            let s1_has_partial = !partial.peek().is_empty();
            let s2_has_partial = multi && !partial2.peek().is_empty();

            if s1_has_partial && let Some(ref ff) = *turn_is_formatted1_shared.peek() {
                ff.set(false);
            }
            if s2_has_partial && let Some(ref ff) = *turn_is_formatted2_shared.peek() {
                ff.set(false);
            }

            if let Some(sess_rc) = session_handle.peek().as_ref()
                && let Some(ref sess) = *sess_rc.borrow()
            {
                let _ = sess.force_endpoint();
            }
            if multi
                && let Some(sess_rc) = session_handle2.peek().as_ref()
                && let Some(ref sess) = *sess_rc.borrow()
            {
                let _ = sess.force_endpoint();
            }

            // 3. Wait for formatted responses (5s safety timeout)
            let deadline = js_sys::Date::now() + 5_000.0;
            loop {
                let s1_done = !s1_has_partial
                    || turn_is_formatted1_shared
                        .peek()
                        .as_ref()
                        .map(|f| f.get())
                        .unwrap_or(true);
                let s2_done = !s2_has_partial
                    || turn_is_formatted2_shared
                        .peek()
                        .as_ref()
                        .map(|f| f.get())
                        .unwrap_or(true);
                if (s1_done && s2_done) || js_sys::Date::now() >= deadline {
                    break;
                }
                gloo_timers::future::TimeoutFuture::new(50).await;
            }

            // 4. Terminate sessions
            if let Some(sess_rc) = session_handle.peek().as_ref()
                && let Some(ref sess) = *sess_rc.borrow()
            {
                let _ = sess.terminate();
            }
            if multi
                && let Some(sess_rc) = session_handle2.peek().as_ref()
                && let Some(ref sess) = *sess_rc.borrow()
            {
                let _ = sess.terminate();
            }

            // 5. Flush speaker 1 partial
            let start = session_start_time.peek().unwrap_or(0.0);
            let ls = last_committed_speaker.peek().clone();
            let p1 = partial.peek().clone();
            if !p1.is_empty() {
                if multi {
                    if let Some(ref live_rc) = *live_turns_shared.peek() {
                        let elapsed = js_sys::Date::now() - start;
                        let new_words = split_turn_to_words(elapsed, &speaker1_name.peek(), &p1);
                        let mut words = live_rc.borrow_mut();
                        for w in new_words {
                            let pos = words.partition_point(|x| x.estimated_ms <= w.estimated_ms);
                            words.insert(pos, w);
                        }
                    }
                } else {
                    let ls_name = ls.as_ref().map(|r| r.borrow().clone()).unwrap_or_default();
                    let name = speaker1_name.peek().clone();
                    let prev = transcript.peek().clone();
                    let elapsed = js_sys::Date::now() - start;
                    let new_text = build_committed_text(
                        &prev,
                        &p1,
                        &name,
                        false,
                        *show_labels.peek(),
                        *show_timestamps.peek(),
                        elapsed,
                        &ls_name,
                    );
                    transcript.set(new_text);
                    if let Some(ref ls_rc) = ls {
                        *ls_rc.borrow_mut() = name;
                    }
                }
                partial.set(String::new());
            }
            // Flush speaker 2 partial
            if multi {
                let p2 = partial2.peek().clone();
                if !p2.is_empty() {
                    if let Some(ref live_rc) = *live_turns_shared.peek() {
                        let elapsed = js_sys::Date::now() - start;
                        let new_words = split_turn_to_words(elapsed, &speaker2_name.peek(), &p2);
                        let mut words = live_rc.borrow_mut();
                        for w in new_words {
                            let pos = words.partition_point(|x| x.estimated_ms <= w.estimated_ms);
                            words.insert(pos, w);
                        }
                    }
                    partial2.set(String::new());
                }
                // Force-graduate all remaining live words
                if let Some(ref live_rc) = *live_turns_shared.peek() {
                    let ls_rc = ls
                        .clone()
                        .unwrap_or_else(|| Rc::new(RefCell::new(String::new())));
                    graduate_live_words(
                        live_rc,
                        &mut transcript,
                        &ls_rc,
                        f64::MAX,
                        f64::MAX,
                        start,
                        *show_labels.peek(),
                        *show_timestamps.peek(),
                    );
                    let previous_live_turns_version = *live_turns_version.peek();
                    live_turns_version.set(previous_live_turns_version + 1);
                }
            }
            // Run paragraph detection (gated by trigger strategy).
            // Authentication lives in the proxy; we always fire the
            // request and let it 412 if no key is configured.
            //
            // The full-transcript stop pass now uses the same picked
            // reformat model as the incremental pass — since
            // `docs/global-reformat-spec.md` §4 unified the picker
            // into one drawer entry. Users who want a higher-quality
            // stop pass simply pick Sonnet (or the equivalent
            // registry id) themselves.
            let do_auto_format = *auto_format_enabled.peek();
            let do_format_on_stop = *format_on_stop.peek();
            let model_for_stop = reformat_model_config_id.peek().clone();
            let cred_for_stop = reformat_credential.peek().clone();
            if do_auto_format {
                let model = model_for_stop.clone();
                let cred = cred_for_stop.clone();
                let depth = *format_depth.peek();
                let ctx_depth = *format_context_depth.peek();
                let mut t = transcript;
                let mut llm = llm_cost;
                spawn_browser_task(async move {
                    let text = t.peek().clone();
                    if !text.is_empty()
                        && let Some(result) =
                            check_formatting(&text, multi, &model, &cred, depth, ctx_depth).await
                    {
                        let (in_rate, out_rate) = llm_rates(&model);
                        let previous_llm_cost = *llm.peek();
                        llm.set(
                            previous_llm_cost
                                + token_cost(
                                    result.input_tokens,
                                    result.output_tokens,
                                    in_rate,
                                    out_rate,
                                ),
                        );
                        if let Some(formatted) = result.formatted {
                            let cursor = get_cursor();
                            t.set(formatted);
                            if let Some((s, e)) = cursor {
                                restore_cursor(s, e);
                            }
                        }
                    }
                    // Full-transcript pass on stop using the same picked model.
                    if do_format_on_stop {
                        let text = t.peek().clone();
                        if !text.is_empty()
                            && let Some(result) =
                                check_formatting(&text, multi, &model, &cred, 0, 0).await
                        {
                            let (in_rate, out_rate) = llm_rates(&model);
                            let previous_llm_cost = *llm.peek();
                            llm.set(
                                previous_llm_cost
                                    + token_cost(
                                        result.input_tokens,
                                        result.output_tokens,
                                        in_rate,
                                        out_rate,
                                    ),
                            );
                            if let Some(formatted) = result.formatted {
                                let cursor = get_cursor();
                                t.set(formatted);
                                if let Some((s, e)) = cursor {
                                    restore_cursor(s, e);
                                }
                            }
                        }
                    }
                });
            } else if do_format_on_stop {
                // Auto-format disabled — skip incremental but still do the full pass
                let model = model_for_stop;
                let cred = cred_for_stop;
                let mut t = transcript;
                let mut llm = llm_cost;
                spawn_browser_task(async move {
                    let text = t.peek().clone();
                    if !text.is_empty()
                        && let Some(result) =
                            check_formatting(&text, multi, &model, &cred, 0, 0).await
                    {
                        let (in_rate, out_rate) = llm_rates(&model);
                        let previous_llm_cost = *llm.peek();
                        llm.set(
                            previous_llm_cost
                                + token_cost(
                                    result.input_tokens,
                                    result.output_tokens,
                                    in_rate,
                                    out_rate,
                                ),
                        );
                        if let Some(formatted) = result.formatted {
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
        }); // end spawn
    };

    // ── Continue ────────────────────────────────────────────────────
    let on_continue = move |_: Event<MouseData>| {
        start_recording();
    };

    // ── Transfer action ─────────────────────────────────────────────
    let on_transfer = move |_| {
        let text = transcript.peek().clone();
        if text.is_empty() {
            return;
        }
        let mode = *transfer_mode.peek();
        match mode {
            TransferMode::Copy => {
                if let Some(window) = web_sys::window() {
                    let clipboard = window.navigator().clipboard();
                    let _ = clipboard.write_text(&text);
                    transfer_feedback.set(Some("✓ Copied".into()));
                    spawn_browser_task(async move {
                        gloo_timers::future::TimeoutFuture::new(2_000).await;
                        transfer_feedback.set(None);
                    });
                }
            }
            TransferMode::TxtFile => {
                let should_prompt_filename = *prompt_filename.peek();
                let filename = if should_prompt_filename {
                    let default = generate_filename("txt");
                    match web_sys::window()
                        .and_then(|w| w.prompt_with_message_and_default("Save as:", &default).ok())
                        .flatten()
                    {
                        Some(f) if !f.is_empty() => f,
                        _ => return,
                    }
                } else {
                    generate_filename("txt")
                };
                trigger_download(&filename, &text, "text/plain");
                let fb = if should_prompt_filename {
                    format!("✓ Saved as {filename}")
                } else {
                    "✓ Saved".into()
                };
                transfer_feedback.set(Some(fb));
                spawn_browser_task(async move {
                    gloo_timers::future::TimeoutFuture::new(2_000).await;
                    transfer_feedback.set(None);
                });
            }
            TransferMode::MdFile => {
                let multi = *speaker2_enabled.peek();
                let duration_ms = (*session_start_time.peek())
                    .map(|st| js_sys::Date::now() - st)
                    .unwrap_or(0.0);
                let md_content = if multi {
                    let s1 = speaker1_name.peek().clone();
                    let s2 = speaker2_name.peek().clone();
                    let date = js_sys::Date::new_0();
                    let iso = date.to_iso_string();
                    format!(
                        "---\ntitle: Parley Transcript\ndate: {}\nspeakers:\n  - {}\n  - {}\nduration: \"{}\"\n---\n\n{}",
                        String::from(iso),
                        s1,
                        s2,
                        format_duration(duration_ms),
                        text
                    )
                } else {
                    let date = js_sys::Date::new_0();
                    let iso = date.to_iso_string();
                    format!(
                        "# Parley Transcript\n\nDate: {}\n\n{}",
                        String::from(iso),
                        text
                    )
                };
                let should_prompt_filename = *prompt_filename.peek();
                let filename = if should_prompt_filename {
                    let default = generate_filename("md");
                    match web_sys::window()
                        .and_then(|w| w.prompt_with_message_and_default("Save as:", &default).ok())
                        .flatten()
                    {
                        Some(f) if !f.is_empty() => f,
                        _ => return,
                    }
                } else {
                    generate_filename("md")
                };
                trigger_download(&filename, &md_content, "text/markdown");
                let fb = if should_prompt_filename {
                    format!("✓ Saved as {filename}")
                } else {
                    "✓ Saved".into()
                };
                transfer_feedback.set(Some(fb));
                spawn_browser_task(async move {
                    gloo_timers::future::TimeoutFuture::new(2_000).await;
                    transfer_feedback.set(None);
                });
            }
            TransferMode::VsCode => {
                // Future: VS Code extension bridge
            }
        }
    };

    // ── Clear ───────────────────────────────────────────────────────
    let on_clear = move |_| {
        show_clear_confirm.set(true);
    };
    let on_cancel_clear = move |_| {
        show_clear_confirm.set(false);
    };
    let on_confirm_clear = move |_| {
        show_clear_confirm.set(false);
        transcript.set(String::new());
        partial.set(String::new());
        partial2.set(String::new());
        if let Some(ct) = current_turn_shared.peek().as_ref() {
            *ct.borrow_mut() = String::new();
        }
        if let Some(cto) = current_turn_order_shared.peek().as_ref() {
            cto.set(u32::MAX);
        }
        if let Some(ct) = current_turn_shared2.peek().as_ref() {
            *ct.borrow_mut() = String::new();
        }
        if let Some(cto) = current_turn_order_shared2.peek().as_ref() {
            cto.set(u32::MAX);
        }
        // Clear live zone
        if let Some(ref live_rc) = *live_turns_shared.peek() {
            live_rc.borrow_mut().clear();
            let previous_live_turns_version = *live_turns_version.peek();
            live_turns_version.set(previous_live_turns_version + 1);
        }
        // Reset cost counters
        stt_cost.set(0.0);
        llm_cost.set(0.0);
        if *rec_state.peek() == RecState::Stopped {
            rec_state.set(RecState::Idle);
            status_msg.set("Ready".into());
        }
    };

    // ── Reformat (on-demand, full transcript) ───────────────────────
    let on_reformat = move |_: Event<MouseData>| {
        // Use the user's configured reformat model (Settings →
        // Reformatting). The legacy "always Sonnet 4.6" hardcode was
        // removed when the picker moved to a single global home
        // (`docs/global-reformat-spec.md` §4).
        let model = reformat_model_config_id.peek().clone();
        let cred = reformat_credential.peek().clone();
        let multi = *speaker2_enabled.peek();
        let mut t = transcript;
        let mut r = reformatting;
        let mut llm = llm_cost;
        spawn_browser_task(async move {
            r.set(true);
            let text = t.peek().clone();
            if !text.is_empty()
                && let Some(result) = check_formatting(&text, multi, &model, &cred, 0, 0).await
            {
                let (in_rate, out_rate) = llm_rates(&model);
                let previous_llm_cost = *llm.peek();
                llm.set(
                    previous_llm_cost
                        + token_cost(result.input_tokens, result.output_tokens, in_rate, out_rate),
                );
                if let Some(formatted) = result.formatted {
                    let cursor = get_cursor();
                    t.set(formatted);
                    if let Some((s, e)) = cursor {
                        restore_cursor(s, e);
                    }
                }
            }
            r.set(false);
        });
    };

    // ── Derived values ──────────────────────────────────────────────
    let state = rec_state();
    let multi = (speaker2_enabled)();

    // Build live zone text for rendering (only in multi mode)
    let _ltv = (live_turns_version)(); // subscribe to changes
    let live_zone_lines: Vec<String> = if multi {
        if let Some(ref live_rc) = (live_turns_shared)() {
            render_live_zone(&live_rc.borrow(), (show_labels)(), (show_timestamps)())
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    let has_live = !live_zone_lines.is_empty();

    let has_text = !(transcript)().is_empty()
        || !(partial)().is_empty()
        || (multi && !(partial2)().is_empty())
        || has_live;
    let is_stopping = state == RecState::Stopping;
    let can_record = state == RecState::Idle || (state == RecState::Stopped && !has_text);
    let can_stop = state == RecState::Recording;
    let can_end_turn = state == RecState::Recording && !multi;
    let can_continue = state == RecState::Stopped && has_text;
    let can_transfer = has_text && !is_stopping;
    let can_clear = has_text && !is_stopping;
    let can_reformat = anthropic_configured()
        && has_text
        && !reformatting()
        && !is_stopping
        && !(reformat_model_config_id)().is_empty();
    let s1_name_val = (speaker1_name)();
    let s2_name_val = (speaker2_name)();
    let tm = (transfer_mode)();
    let fb = (transfer_feedback)();

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
                // Gear button moved to Root so it sits next to the
                // Transcribe / Conversation tab buttons; the drawer is
                // rendered at the shell level via SettingsDrawer.
            }

            // ── Error banner ────────────────────────────────────────
            if let Some(ref err) = (error_msg)() {
                div { class: "error-banner",
                    span { "{err}" }
                    button {
                        class: "error-dismiss",
                        onclick: move |_| error_msg.set(None),
                        "\u{2715}"
                    }
                }
            }

            // ── Transcript (editable) ───────────────────────────────
            div { class: "transcript-area",
                textarea {
                    id: TEXTAREA_ID,
                    class: "transcript-edit",
                    placeholder: "Transcribed text will appear here\u{2026}",
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

            // ── Live zone (multi-speaker chrono-sorted pending turns) ──
            if has_live {
                div { class: "live-zone",
                    div { class: "live-zone-label", "Live \u{2014} sorting\u{2026}" }
                    for line in live_zone_lines.iter() {
                        p { class: "live-zone-line", "{line}" }
                    }
                }
            }

            // ── Current turn boxes ──────────────────────────────────
            if multi {
                // Dual current-turn layout
                div { class: "current-turns-row",
                    div { class: "current-turn current-turn-half",
                        if state == RecState::Recording {
                            span { class: "current-turn-label", "{s1_name_val} speaking\u{2026}" }
                            p { class: "current-turn-text",
                                if (partial)().is_empty() {
                                    "\u{2026}"
                                } else {
                                    "{partial}"
                                }
                            }
                        } else {
                            span { class: "current-turn-label current-turn-idle", "{s1_name_val}" }
                            p { class: "current-turn-text current-turn-placeholder",
                                "Not recording"
                            }
                        }
                        button {
                            class: "btn btn-endturn-inline",
                            disabled: state != RecState::Recording,
                            onclick: on_end_turn1,
                            "\u{23ce} End Turn"
                        }
                    }
                    div { class: "current-turn current-turn-half",
                        if state == RecState::Recording {
                            span { class: "current-turn-label", "{s2_name_val} speaking\u{2026}" }
                            p { class: "current-turn-text",
                                if (partial2)().is_empty() {
                                    "\u{2026}"
                                } else {
                                    "{partial2}"
                                }
                            }
                        } else {
                            span { class: "current-turn-label current-turn-idle", "{s2_name_val}" }
                            p { class: "current-turn-text current-turn-placeholder",
                                "Not recording"
                            }
                        }
                        button {
                            class: "btn btn-endturn-inline",
                            disabled: state != RecState::Recording,
                            onclick: on_end_turn2,
                            "\u{23ce} End Turn"
                        }
                    }
                }
            } else {
                // Single current-turn layout
                div { class: "current-turn",
                    if state == RecState::Recording {
                        span { class: "current-turn-label", "Speaking\u{2026}" }
                        p { class: "current-turn-text",
                            if (partial)().is_empty() {
                                "\u{2026}"
                            } else {
                                "{partial}"
                            }
                        }
                    } else {
                        span { class: "current-turn-label current-turn-idle", "Current turn" }
                        p { class: "current-turn-text current-turn-placeholder", "Not recording" }
                    }
                }
            }

            // ── Button bar ──────────────────────────────────────────
            div { class: "button-bar",
                button {
                    class: "btn btn-record",
                    disabled: !can_record,
                    onclick: on_record,
                    "\u{25cf} Record"
                }
                button {
                    class: "btn btn-stop",
                    disabled: !can_stop,
                    onclick: on_stop,
                    "\u{25a0} Stop"
                }
                button {
                    class: "btn btn-continue",
                    disabled: !can_continue,
                    onclick: on_continue,
                    "\u{25cf} Continue"
                }
                button {
                    class: "btn btn-endturn",
                    disabled: !can_end_turn,
                    onclick: on_end_turn1,
                    "\u{23ce} End Turn"
                }
                // Transfer combo button
                div { class: "transfer-combo",
                    button {
                        class: "btn btn-transfer-main",
                        disabled: !can_transfer,
                        onclick: on_transfer,
                        if let Some(ref feedback) = fb {
                            "{feedback}"
                        } else {
                            "{tm.label()}"
                        }
                    }
                    button {
                        class: "btn btn-transfer-arrow",
                        disabled: !can_transfer,
                        onclick: move |_| {
                            let is_open = *show_transfer_menu.peek();
                            show_transfer_menu.set(!is_open);
                        },
                        "\u{25be}"
                    }
                    if (show_transfer_menu)() && can_transfer {
                        div {
                            class: "transfer-overlay",
                            onclick: move |_| show_transfer_menu.set(false),
                        }
                        div { class: "transfer-menu",
                            button {
                                class: if tm == TransferMode::Copy { "transfer-option active" } else { "transfer-option" },
                                onclick: move |_| {
                                    transfer_mode.set(TransferMode::Copy);
                                    save("parley_transfer_mode", TransferMode::Copy.cookie_value());
                                    show_transfer_menu.set(false);
                                },
                                if tm == TransferMode::Copy {
                                    "\u{2713} Copy"
                                } else {
                                    "  Copy"
                                }
                            }
                            button {
                                class: if tm == TransferMode::TxtFile { "transfer-option active" } else { "transfer-option" },
                                onclick: move |_| {
                                    transfer_mode.set(TransferMode::TxtFile);
                                    save("parley_transfer_mode", TransferMode::TxtFile.cookie_value());
                                    show_transfer_menu.set(false);
                                },
                                if tm == TransferMode::TxtFile {
                                    "\u{2713} TXT File"
                                } else {
                                    "  TXT File"
                                }
                            }
                            button {
                                class: if tm == TransferMode::MdFile { "transfer-option active" } else { "transfer-option" },
                                onclick: move |_| {
                                    transfer_mode.set(TransferMode::MdFile);
                                    save("parley_transfer_mode", TransferMode::MdFile.cookie_value());
                                    show_transfer_menu.set(false);
                                },
                                if tm == TransferMode::MdFile {
                                    "\u{2713} MD File"
                                } else {
                                    "  MD File"
                                }
                            }
                            button {
                                class: "transfer-option disabled",
                                disabled: true,
                                "  VS Code (coming soon)"
                            }
                            hr { class: "transfer-divider" }
                            label { class: "transfer-option transfer-checkbox",
                                input {
                                    r#type: "checkbox",
                                    checked: "{prompt_filename}",
                                    onchange: move |evt: Event<FormData>| {
                                        let v = evt.checked();
                                        prompt_filename.set(v);
                                        save("parley_prompt_filename", if v { "true" } else { "false" });
                                    },
                                }
                                "Prompt for filename"
                            }
                        }
                    }
                }
                button {
                    class: "btn btn-clear",
                    disabled: !can_clear,
                    onclick: on_clear,
                    "Clear"
                }
                // The combo `▾` dropdown was removed when the picker
                // moved into Settings → Reformatting (single global
                // home, applies to both Transcribe and Conversation
                // modes). Spec `docs/global-reformat-spec.md` §4.
                button {
                    class: "btn btn-reformat-main",
                    style: "border-radius: 8px;",
                    onclick: on_reformat,
                    disabled: !can_reformat,
                    if reformatting() {
                        "Reformatting\u{2026}"
                    } else {
                        "\u{00b6} Reformat"
                    }
                }
            }

            if (show_clear_confirm)() {
                div { class: "confirm-overlay",
                    div {
                        class: "confirm-dialog",
                        role: "dialog",
                        aria_modal: "true",
                        aria_labelledby: "clear-confirm-title",
                        tabindex: "0",
                        onkeydown: move |evt| {
                            let key = evt.key();
                            if matches!(key, Key::Enter | Key::Escape) {
                                evt.prevent_default();
                                show_clear_confirm.set(false);
                            }
                        },
                        h2 { id: "clear-confirm-title", "Clear transcript?" }
                        p { "This will remove the current transcript and live turn text." }
                        div { class: "confirm-actions",
                            button {
                                id: CLEAR_CONFIRM_CANCEL_ID,
                                class: "btn btn-confirm-cancel",
                                autofocus: true,
                                onclick: on_cancel_clear,
                                "Cancel"
                            }
                            button {
                                class: "btn btn-clear",
                                onclick: on_confirm_clear,
                                "Clear"
                            }
                        }
                    }
                }
            }

            // ── Status bar ──────────────────────────────────────────
            div { class: "status-bar",
                div { class: "status-left",
                    span { class: if state == RecState::Recording { "status-dot recording" } else { "status-dot" } }
                    span { "{status_msg}" }
                }
                if (show_cost_meter)() && (stt_cost() > 0.0 || llm_cost() > 0.0) {
                    div {
                        class: "cost-meter",
                        title: "STT: ${format_cost(stt_cost())} + LLM: ${format_cost(llm_cost())}",
                        span { class: "cost-meter-label", "$" }
                        span { class: "cost-meter-value", "{format_cost(stt_cost() + llm_cost())}" }
                    }
                }
            }

            // Settings drawer is now rendered at the Root shell level
            // by `crate::ui::settings_drawer::SettingsDrawer`, so the
            // gear button in `Root` works regardless of whether
            // Transcribe or Conversation is the active tab.
        }
    }
}

/// Settings-drawer row for managing one provider's `default`
/// credential via the proxy's `/api/secrets` surface.
///
/// Renders three states:
/// - **Unconfigured**: shows a password input + "Save" button.
/// - **Configured**: shows a "configured" badge with "Replace" and
///   "Remove" buttons; "Replace" toggles the input back on.
/// - **Error**: shows the proxy's error message inline so the user
///   can retry.
///
/// Calls `on_changed` after any successful mutation so the parent
/// can refresh its `use_secrets_status` resource.
#[component]
pub(crate) fn SecretsKeyRow(
    provider: &'static str,
    label: &'static str,
    hint: &'static str,
    configured: bool,
    on_changed: EventHandler<()>,
) -> Element {
    let mut value = use_signal(String::new);
    let mut editing = use_signal(|| !configured);
    let mut error = use_signal(String::new);
    let mut busy = use_signal(|| false);

    // When the parent flips `configured` (e.g. after a delete from
    // another row), collapse back to the badge view automatically.
    use_effect(use_reactive!(|configured| {
        defer_ui_update(move || {
            editing.set(!configured);
            if configured {
                value.set(String::new());
            }
        });
    }));

    let input_id = format!("secret-{provider}");
    let on_save = move |_| {
        let key = value.peek().clone();
        if key.trim().is_empty() {
            error.set("Key cannot be empty".to_string());
            return;
        }
        busy.set(true);
        error.set(String::new());
        spawn_browser_task(async move {
            match secrets::set_credential(provider, "default", &key).await {
                Ok(_) => {
                    value.set(String::new());
                    editing.set(false);
                    on_changed.call(());
                }
                Err(e) => error.set(e),
            }
            busy.set(false);
        });
    };
    let on_remove = move |_| {
        busy.set(true);
        error.set(String::new());
        spawn_browser_task(async move {
            match secrets::delete_credential(provider, "default").await {
                Ok(()) => {
                    on_changed.call(());
                }
                Err(e) => error.set(e),
            }
            busy.set(false);
        });
    };

    rsx! {
        label { r#for: "{input_id}", "{label}" }
        if editing() {
            div { class: "settings-row",
                input {
                    id: "{input_id}",
                    r#type: "password",
                    class: "settings-input",
                    placeholder: "Enter your API key\u{2026}",
                    value: "{value}",
                    disabled: busy(),
                    oninput: move |evt: Event<FormData>| value.set(evt.value()),
                }
                button {
                    class: "btn",
                    disabled: busy(),
                    onclick: on_save,
                    if busy() { "Saving\u{2026}" } else { "Save" }
                }
                if configured {
                    button {
                        class: "btn",
                        disabled: busy(),
                        onclick: move |_| editing.set(false),
                        "Cancel"
                    }
                }
            }
        } else {
            div { class: "settings-row",
                span { class: "settings-badge", "Configured" }
                button {
                    class: "btn",
                    disabled: busy(),
                    onclick: move |_| editing.set(true),
                    "Replace"
                }
                button {
                    class: "btn",
                    disabled: busy(),
                    onclick: on_remove,
                    if busy() { "Removing\u{2026}" } else { "Remove" }
                }
            }
        }
        p { class: "settings-hint", "{hint}" }
        if !error().is_empty() {
            p { class: "settings-error", "{error}" }
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

/* Countdown */
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

/* Live zone */
.live-zone {
    background: #12284a;
    border-radius: 10px;
    padding: 0.75rem 1.25rem;
    margin-bottom: 1rem;
    border-left: 3px solid #e9a545;
    max-height: 20vh;
    overflow-y: auto;
    line-height: 1.7;
    font-size: 1.05rem;
}
.live-zone-label {
    display: block;
    font-size: 0.7rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: #e9a545;
    margin-bottom: 0.3rem;
}
.live-zone-line {
    color: #c0c0d0;
    margin: 0.15rem 0;
}

/* Current turn boxes */
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
.current-turns-row {
    display: flex;
    gap: 1rem;
    margin-bottom: 1.5rem;
}
.current-turn-half {
    flex: 1;
    margin-bottom: 0;
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
.current-turn-idle { color: #8888aa; }
.current-turn-placeholder { color: #555570; }
.btn-endturn-inline {
    margin-top: 0.6rem;
    padding: 0.35rem 0.8rem;
    border: none;
    border-radius: 6px;
    font-size: 0.8rem;
    font-weight: 600;
    cursor: pointer;
    background: #16213e;
    color: #4ecca3;
    transition: background 0.15s;
}
.btn-endturn-inline:hover { background: #1a4a7a; }

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
    align-items: flex-start;
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
.btn:active:not(:disabled) { transform: scale(0.97); }
.btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
}

.btn-record { background: #e94560; color: #fff; }
.btn-record:hover:not(:disabled) { background: #ff6b81; }
.btn-stop { background: #e94560; color: #fff; }
.btn-stop:hover:not(:disabled) { background: #ff6b81; }
.btn-continue { background: #4ecca3; color: #1a1a2e; }
.btn-continue:hover:not(:disabled) { background: #6ee6bb; }
.btn-endturn { background: #0f3460; color: #e0e0e0; }
.btn-endturn:hover:not(:disabled) { background: #1a4a7a; }
.btn-clear { background: transparent; color: #8888aa; border: 1px solid #8888aa; }
.btn-clear:hover:not(:disabled) { color: #e0e0e0; border-color: #e0e0e0; }
.btn-confirm-cancel { background: #0f3460; color: #e0e0e0; }
.btn-confirm-cancel:hover:not(:disabled) { background: #1a4a7a; }
/* Format combo button */
.format-combo {
    position: relative;
    display: inline-flex;
}
.btn-reformat-main {
    background: transparent;
    color: #b8a9c9;
    border: 1px solid #b8a9c9;
    border-radius: 8px 0 0 8px;
    padding: 0.65rem 1rem;
}
.btn-reformat-main:hover:not(:disabled) { color: #e0d0f0; border-color: #e0d0f0; }
.btn-reformat-arrow {
    background: transparent;
    color: #b8a9c9;
    border: 1px solid #b8a9c9;
    border-left: none;
    border-radius: 0 8px 8px 0;
    padding: 0.65rem 0.5rem;
    font-size: 0.85rem;
}
.btn-reformat-arrow:hover { color: #e0d0f0; border-color: #e0d0f0; }
.format-overlay {
    position: fixed;
    top: 0; left: 0; right: 0; bottom: 0;
    z-index: 90;
}
.format-menu {
    position: absolute;
    bottom: 110%;
    right: 0;
    min-width: 260px;
    background: #16213e;
    border: 1px solid #0f3460;
    border-radius: 8px;
    box-shadow: 0 4px 16px rgba(0,0,0,0.4);
    z-index: 91;
    padding: 0.8rem 1rem;
}
.format-menu-title {
    font-weight: 700;
    color: #e0e0e0;
    margin-bottom: 0.6rem;
    font-size: 0.95rem;
}
.format-menu label {
    display: block;
    color: #b0b0cc;
    font-size: 0.82rem;
    margin-top: 0.5rem;
    margin-bottom: 0.2rem;
}
.format-menu .settings-input {
    width: 100%;
    box-sizing: border-box;
}
.format-menu .settings-hint {
    color: #888;
    font-size: 0.78rem;
    margin-top: 0.4rem;
}

/* Transfer combo button */
.transfer-combo {
    position: relative;
    display: inline-flex;
}
.btn-transfer-main {
    background: #0f3460;
    color: #e0e0e0;
    border-radius: 8px 0 0 8px;
    padding: 0.65rem 1rem;
}
.btn-transfer-main:hover:not(:disabled) { background: #1a4a7a; }
.btn-transfer-arrow {
    background: #0f3460;
    color: #e0e0e0;
    border-radius: 0 8px 8px 0;
    padding: 0.65rem 0.5rem;
    border-left: 1px solid #1a1a2e;
    font-size: 0.85rem;
}
.btn-transfer-arrow:hover:not(:disabled) { background: #1a4a7a; }
.confirm-overlay {
    position: fixed;
    inset: 0;
    z-index: 120;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 1rem;
    background: rgba(0,0,0,0.58);
}
.confirm-dialog {
    width: min(420px, 100%);
    background: #16213e;
    border: 1px solid #2a3960;
    border-radius: 8px;
    box-shadow: 0 12px 36px rgba(0,0,0,0.45);
    padding: 1.25rem;
}
.confirm-dialog:focus { outline: 2px solid #4ecca3; outline-offset: 2px; }
.confirm-dialog h2 {
    font-size: 1.1rem;
    color: #e0e0e0;
    margin-bottom: 0.5rem;
}
.confirm-dialog p {
    color: #c0c0d0;
    line-height: 1.5;
    margin-bottom: 1rem;
}
.confirm-actions {
    display: flex;
    justify-content: flex-end;
    gap: 0.75rem;
    flex-wrap: wrap;
}
.transfer-overlay {
    position: fixed;
    top: 0; left: 0; right: 0; bottom: 0;
    z-index: 90;
}
.transfer-menu {
    position: absolute;
    bottom: 110%;
    left: 0;
    min-width: 200px;
    background: #16213e;
    border: 1px solid #0f3460;
    border-radius: 8px;
    box-shadow: 0 4px 16px rgba(0,0,0,0.4);
    z-index: 91;
    padding: 0.3rem 0;
    overflow: hidden;
}
.transfer-option {
    display: block;
    width: 100%;
    text-align: left;
    background: none;
    border: none;
    color: #e0e0e0;
    padding: 0.5rem 1rem;
    font-size: 0.9rem;
    cursor: pointer;
    font-family: monospace;
    white-space: pre;
}
.transfer-option:hover { background: #0f3460; }
.transfer-option.active { color: #4ecca3; }
.transfer-option.disabled {
    color: #555570;
    cursor: default;
}
.transfer-option.disabled:hover { background: none; }
.transfer-divider {
    border: none;
    border-top: 1px solid #0f3460;
    margin: 0.3rem 0;
}
.transfer-checkbox {
    display: flex !important;
    align-items: center;
    gap: 0.5rem;
    cursor: pointer;
    font-family: inherit;
}
.transfer-checkbox input {
    accent-color: #4ecca3;
    cursor: pointer;
}

/* Status bar */
.status-bar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    font-size: 0.85rem;
    color: #8888aa;
}
.status-left {
    display: flex;
    align-items: center;
    gap: 0.5rem;
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

/* Cost meter */
.cost-meter {
    display: flex;
    align-items: center;
    gap: 0.25rem;
    font-size: 0.85rem;
    font-variant-numeric: tabular-nums;
    color: #4ecca3;
    cursor: default;
}
.cost-meter-label {
    font-weight: 600;
    color: #4ecca3;
    opacity: 0.7;
}
.cost-meter-value {
    font-weight: 600;
    letter-spacing: 0.02em;
}

/* Settings option row (outside speaker cards) */
.settings-option-row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    font-size: 0.85rem;
    color: #c0c0d0;
    margin-top: 1.2rem;
    cursor: pointer;
}
.settings-option-row input {
    accent-color: #4ecca3;
    cursor: pointer;
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
    width: 380px;
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
select.settings-input {
    cursor: pointer;
    appearance: auto;
}
.soniox-context-input {
    min-height: 6rem;
    resize: vertical;
    line-height: 1.4;
}
.settings-hint {
    font-size: 0.8rem;
    color: #8888aa;
    margin-top: 0.5rem;
}
.settings-warn { color: #e9a545; }

/* Settings section header */
.settings-section-header {
    font-size: 0.9rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: #4ecca3;
    margin-top: 2rem;
    padding-bottom: 0.5rem;
    border-bottom: 1px solid #0f3460;
}

/* Speaker cards */
.speaker-card {
    background: #1a1a2e;
    border-radius: 8px;
    padding: 1rem;
    margin-top: 1rem;
}
.speaker-card-title {
    font-size: 0.9rem;
    font-weight: 600;
    color: #e0e0e0;
    margin-bottom: 0.75rem;
    display: flex;
    justify-content: space-between;
    align-items: center;
}
.speaker-card-row {
    display: flex;
    gap: 0.75rem;
}
.speaker-field {
    flex: 1;
}
.speaker-field label {
    margin-top: 0;
}

/* Toggle switch */
.toggle-switch {
    position: relative;
    display: inline-block;
    width: 40px;
    height: 22px;
    margin: 0;
}
.toggle-switch input {
    opacity: 0;
    width: 0;
    height: 0;
}
.toggle-slider {
    position: absolute;
    cursor: pointer;
    top: 0; left: 0; right: 0; bottom: 0;
    background: #333;
    border-radius: 22px;
    transition: 0.2s;
}
.toggle-slider::before {
    content: "";
    position: absolute;
    height: 16px;
    width: 16px;
    left: 3px;
    bottom: 3px;
    background: white;
    border-radius: 50%;
    transition: 0.2s;
}
.toggle-switch input:checked + .toggle-slider {
    background: #4ecca3;
}
.toggle-switch input:checked + .toggle-slider::before {
    transform: translateX(18px);
}

/* Speaker options */
.speaker-options {
    margin-top: 0.75rem;
    padding-top: 0.75rem;
    border-top: 1px solid #0f3460;
}
.option-row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    font-size: 0.85rem;
    color: #c0c0d0;
    margin-top: 0.5rem;
    cursor: pointer;
}
.option-row input {
    accent-color: #4ecca3;
    cursor: pointer;
}

.btn-close-settings {
    margin-top: 2rem;
    background: #0f3460;
    color: #e0e0e0;
    width: 100%;
}
.btn-close-settings:hover { background: #1a4a7a; }

/* Secrets / API key rows */
.settings-section-heading {
    font-size: 0.95rem;
    color: #c0c0d0;
    margin-top: 1rem;
    margin-bottom: 0.25rem;
}
.settings-row {
    display: flex;
    gap: 0.5rem;
    align-items: center;
    margin-bottom: 0.25rem;
}
.settings-row .settings-input { flex: 1; }
.settings-badge {
    flex: 1;
    padding: 0.4rem 0.6rem;
    background: #1a4a3a;
    color: #4ecca3;
    border-radius: 4px;
    font-size: 0.85rem;
}
.settings-error {
    color: #e94560;
    font-size: 0.8rem;
    margin-top: 0.25rem;
}
"#;
