use std::cell::{Cell, RefCell};
use std::rc::Rc;

use dioxus::prelude::*;
use wasm_bindgen::JsCast;

use crate::audio::capture::BrowserCapture;
use crate::stt::assemblyai::{AssemblyAiSession, TurnEvent, fetch_temp_token};
use crate::ui::secrets;
use parley_core::word_graph::WordGraph;

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
    model: &str,
    depth: usize,
    context_depth: usize,
) -> Option<FormatResult> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    // ── Full-transcript mode (depth == 0) ───────────────────────────
    if depth == 0 {
        if full_transcript.trim().is_empty() {
            return None;
        }

        web_sys::console::log_1(
            &format!(
                "[parley] format check (full): {} chars, model={}",
                full_transcript.len(),
                model,
            )
            .into(),
        );

        let body = serde_json::json!({
            "context": "",
            "text": full_transcript,
            "multi_speaker": multi_speaker,
            "model": model,
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

        if !parsed["changed"].as_bool().unwrap_or(false) {
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
            "[parley] format check: {} chunks total, {} context chars, {} editable chars, model={}",
            total,
            context_text.len(),
            editable_text.len(),
            model,
        )
        .into(),
    );

    let body = serde_json::json!({
        "context": context_text,
        "text": editable_text,
        "multi_speaker": multi_speaker,
        "model": model,
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
    let mut prev = (transcript)();
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
    let mut show_settings = use_signal(|| false);

    // Secrets-status hook: source of truth for whether Anthropic and
    // AssemblyAI have a `default` credential configured. Replaces
    // the old cookie-backed `api_key` / `anthropic_key` signals;
    // values themselves never reach the browser, only configuration
    // status. Mutations (set/delete from Settings) call
    // `secrets_refresh.refresh()` so the gates below re-derive.
    let (secrets_status, mut secrets_refresh) = secrets::use_secrets_status();
    let anthropic_configured = use_memo(move || {
        matches!(
            &*secrets_status.read_unchecked(),
            Some(Ok(s)) if s
                .provider("anthropic")
                .and_then(|p| p.credential("default"))
                .map(|c| c.configured)
                .unwrap_or(false),
        )
    });
    let assemblyai_configured = use_memo(move || {
        matches!(
            &*secrets_status.read_unchecked(),
            Some(Ok(s)) if s
                .provider("assemblyai")
                .and_then(|p| p.credential("default"))
                .map(|c| c.configured)
                .unwrap_or(false),
        )
    });
    // ElevenLabs powers Conversation Mode TTS. The credential is
    // optional — without it, conversation runs text-only and the
    // Voice/Type toggle defaults to Type. Surfaced in Settings so
    // the user can configure or remove the key alongside the
    // other providers.
    let elevenlabs_configured = use_memo(move || {
        matches!(
            &*secrets_status.read_unchecked(),
            Some(Ok(s)) if s
                .provider("elevenlabs")
                .and_then(|p| p.credential("default"))
                .map(|c| c.configured)
                .unwrap_or(false),
        )
    });
    // xAI covers both STT (`grok-stt`) and TTS (`grok-tts`) under one
    // bearer token. Spec: docs/xai-speech-integration-spec.md.
    let xai_configured = use_memo(move || {
        matches!(
            &*secrets_status.read_unchecked(),
            Some(Ok(s)) if s
                .provider("xai")
                .and_then(|p| p.credential("default"))
                .map(|c| c.configured)
                .unwrap_or(false),
        )
    });

    // ── Pipeline picks ──────────────────────────────────────────────
    // Single source of truth for STT/TTS provider + voice. Both this
    // view and the conversation view read these via
    // `crate::ui::pipeline`. Default values mirror what the proxy
    // selects when no client preference is sent (AssemblyAI for STT,
    // ElevenLabs for TTS).
    let mut pipeline_stt_provider = use_signal(|| {
        load(crate::ui::pipeline::STT_PROVIDER_KEY).unwrap_or_else(|| "assemblyai".to_string())
    });
    let mut pipeline_tts_provider = use_signal(|| {
        load(crate::ui::pipeline::TTS_PROVIDER_KEY).unwrap_or_else(|| "elevenlabs".to_string())
    });
    let mut pipeline_tts_voice = use_signal(|| {
        let v = load(crate::ui::pipeline::TTS_VOICE_KEY).unwrap_or_default();
        web_sys::console::log_1(&format!("[app] init voice from cookie: {v:?}").into());
        v
    });
    // Persist changes to cookies. `use_effect(use_reactive!(...))`
    // fires whenever the signal changes; cheap-enough to write on
    // every change since the user only edits these from a dropdown.
    use_effect(use_reactive!(|pipeline_stt_provider| {
        save(
            crate::ui::pipeline::STT_PROVIDER_KEY,
            &pipeline_stt_provider(),
        );
    }));
    use_effect(use_reactive!(|pipeline_tts_provider| {
        save(
            crate::ui::pipeline::TTS_PROVIDER_KEY,
            &pipeline_tts_provider(),
        );
    }));
    // Note: invalidating `pipeline_tts_voice` when the provider
    // changes is handled by the `<select>` onchange handler, not
    // here. `use_effect(use_reactive!)` also fires on initial mount,
    // and clearing the voice on first render would discard the saved
    // pick from cookies before the VoicePicker could honor it.
    use_effect(use_reactive!(|pipeline_tts_voice| {
        let v = pipeline_tts_voice();
        web_sys::console::log_1(&format!("[app] save voice cookie: {v:?}").into());
        save(crate::ui::pipeline::TTS_VOICE_KEY, &v);
    }));
    // Credential the VoicePicker queries the proxy with. Today every
    // pick uses `default`; named-credential support could replace this
    // with a per-provider dropdown later. Defined unconditionally so
    // the hook order is stable across renders even when the Voice row
    // is hidden (TTS = Off).
    let voice_credential = use_signal(|| "default".to_string());

    let mut idle_minutes = use_signal(|| {
        load("parley_idle_minutes")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(5)
    });
    let mut countdown_secs: Signal<Option<u32>> = use_signal(|| None);
    let mut auto_scroll = use_signal(|| true);

    // ── Speaker settings ────────────────────────────────────────────
    let mut speaker1_name =
        use_signal(|| load("parley_speaker1_name").unwrap_or_else(|| "Me".to_string()));
    let mut speaker2_name =
        use_signal(|| load("parley_speaker2_name").unwrap_or_else(|| "Remote".to_string()));
    let mut speaker1_source =
        use_signal(|| load("parley_speaker1_source").unwrap_or_else(|| "mic".to_string()));
    let mut speaker2_source =
        use_signal(|| load("parley_speaker2_source").unwrap_or_else(|| "system".to_string()));
    let mut speaker2_enabled = use_signal(|| {
        load("parley_speaker2_enabled")
            .map(|v| v == "true")
            .unwrap_or(false)
    });
    let mut show_labels = use_signal(|| {
        load("parley_show_labels")
            .map(|v| v == "true")
            .unwrap_or(true)
    });
    let mut show_timestamps = use_signal(|| {
        load("parley_show_timestamps")
            .map(|v| v == "true")
            .unwrap_or(false)
    });

    // ── Transfer / export ───────────────────────────────────────────
    let mut transfer_mode = use_signal(|| {
        load("parley_transfer_mode")
            .map(|s| TransferMode::from_cookie(&s))
            .unwrap_or(TransferMode::Copy)
    });
    let mut show_transfer_menu = use_signal(|| false);
    let mut show_format_menu = use_signal(|| false);
    let mut prompt_filename = use_signal(|| {
        load("parley_prompt_filename")
            .map(|v| v == "true")
            .unwrap_or(false)
    });
    let mut transfer_feedback: Signal<Option<String>> = use_signal(|| None);

    // ── Formatting settings ─────────────────────────────────────────
    let mut format_model = use_signal(|| {
        load("parley_format_model").unwrap_or_else(|| "claude-haiku-4-5-20251001".to_string())
    });
    let mut auto_format_enabled = use_signal(|| {
        load("parley_auto_format")
            .map(|s| s != "false")
            .unwrap_or(true)
    });
    let mut format_nth = use_signal(|| {
        load("parley_format_nth")
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(3)
    });
    let mut format_depth = use_signal(|| {
        load("parley_format_depth")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(2)
    });
    let mut format_context_depth = use_signal(|| {
        load("parley_format_context_depth")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(1)
    });
    let mut format_on_stop = use_signal(|| {
        load("parley_format_on_stop")
            .map(|s| s == "true")
            .unwrap_or(true)
    });
    let reformatting = use_signal(|| false);

    // ── Cost tracking ───────────────────────────────────────────────
    let mut show_cost_meter = use_signal(|| {
        load("parley_show_cost_meter")
            .map(|v| v != "false")
            .unwrap_or(true) // on by default
    });
    // Accumulated STT cost in dollars (updated by ticker based on elapsed time)
    let mut stt_cost = use_signal(|| 0.0_f64);
    // Accumulated LLM cost in dollars (updated after each formatting call)
    let mut llm_cost = use_signal(|| 0.0_f64);

    // ── Speaker 1 handles ───────────────────────────────────────────
    let capture_handle: Signal<Option<Rc<RefCell<Option<BrowserCapture>>>>> = use_signal(|| None);
    let session_handle: Signal<Option<Rc<RefCell<Option<AssemblyAiSession>>>>> =
        use_signal(|| None);
    let mut current_turn_shared: Signal<Option<Rc<RefCell<String>>>> = use_signal(|| None);
    let mut current_turn_order_shared: Signal<Option<Rc<Cell<u32>>>> = use_signal(|| None);
    let mut needs_paragraph_check_shared: Signal<Option<Rc<Cell<bool>>>> = use_signal(|| None);
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

        let multi = (speaker2_enabled)();
        let s1_source = (speaker1_source)();
        let s2_source = (speaker2_source)();

        spawn(async move {
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
            let needs_para_check: Rc<Cell<bool>> = Rc::new(Cell::new(false));
            let turn_commit_counter: Rc<Cell<u32>> = Rc::new(Cell::new(0));
            // Track when the current turn started speaking
            let speech_start1: Rc<Cell<f64>> = Rc::new(Cell::new(0.0));
            let formatted_flag1: Rc<Cell<bool>> = Rc::new(Cell::new(false));

            current_turn_shared.set(Some(current_turn1.clone()));
            current_turn_order_shared.set(Some(current_turn_order1.clone()));
            needs_paragraph_check_shared.set(Some(needs_para_check.clone()));
            turn_is_formatted1_shared.set(Some(formatted_flag1.clone()));

            let session1 = {
                let last_activity = last_activity.clone();
                let last_speaker = last_speaker.clone();
                let ct = current_turn1.clone();
                let cto = current_turn_order1.clone();
                let npc = needs_para_check.clone();
                let tcc = turn_commit_counter.clone();
                let mut t_sig = transcript;
                let mut p_sig = partial;
                let ss1 = speech_start1.clone();
                let ts1 = turn_start1.clone();
                let live_rc = live_turns_rc.clone();
                let s2_for_force = session2_rc_for_s1.clone();
                let mut ltv = live_turns_version;
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
                            ..
                        } = event;
                        last_activity.set(js_sys::Date::now());
                        let text = text.replace('\n', " ").replace("  ", " ");

                        let prev_order = cto.get();
                        let is_new_turn = turn_order != prev_order;

                        if is_new_turn && prev_order != u32::MAX {
                            let old_turn = ct.borrow().clone();
                            if !old_turn.is_empty() {
                                let name = (speaker1_name)();
                                if multi {
                                    // Cross-session force endpoint
                                    if let Some(ref sess) = *s2_for_force.borrow() {
                                        let _ = sess.force_endpoint();
                                    }
                                    // Insert words into live zone at chrono positions
                                    let elapsed = ss1.get();
                                    let new_words = split_turn_to_words(elapsed, &name, &old_turn);
                                    let mut words = live_rc.borrow_mut();
                                    for w in new_words {
                                        let pos = words
                                            .partition_point(|x| x.estimated_ms <= w.estimated_ms);
                                        words.insert(pos, w);
                                    }
                                    drop(words);
                                    ltv.set(ltv() + 1);
                                } else {
                                    // Single speaker: direct to transcript
                                    let cursor = get_cursor();
                                    let prev = (t_sig)();
                                    let elapsed = js_sys::Date::now() - start_time;
                                    let new_text = build_committed_text(
                                        &prev,
                                        &old_turn,
                                        &name,
                                        false,
                                        (show_labels)(),
                                        (show_timestamps)(),
                                        elapsed,
                                        &last_speaker.borrow(),
                                    );
                                    t_sig.set(new_text);
                                    *last_speaker.borrow_mut() = name;
                                    if let Some((s, e)) = cursor {
                                        restore_cursor(s, e);
                                    }
                                }
                                if anthropic_configured() && (auto_format_enabled)() {
                                    tcc.set(tcc.get() + 1);
                                    if tcc.get().is_multiple_of((format_nth)()) {
                                        npc.set(true);
                                    }
                                }
                            }
                        }

                        if is_new_turn {
                            // Record when this new turn started speaking
                            let elapsed = js_sys::Date::now() - start_time;
                            ss1.set(elapsed);
                            ts1.set(elapsed);
                        }

                        cto.set(turn_order);
                        *ct.borrow_mut() = text.clone();
                        p_sig.set(text);
                        ff1.set(is_formatted);
                    },
                    {
                        let mut rec_state = rec_state;
                        let mut status_msg = status_msg;
                        let mut error_msg = error_msg;
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
                                error_msg.set(Some(format!(
                                    "Connection closed (code {code}): {reason}"
                                )));
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
                                let npc = needs_para_check.clone();
                                let tcc = turn_commit_counter.clone();
                                let mut p2_sig = partial2;
                                let ss2 = speech_start2.clone();
                                let ts2 = turn_start2.clone();
                                let live_rc = live_turns_rc.clone();
                                let s1_for_force = session1_rc.clone();
                                let mut ltv = live_turns_version;
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
                                            ..
                                        } = event;
                                        last_activity.set(js_sys::Date::now());
                                        let text = text.replace('\n', " ").replace("  ", " ");

                                        let prev_order = cto.get();
                                        let is_new_turn = turn_order != prev_order;

                                        if is_new_turn && prev_order != u32::MAX {
                                            let old_turn = ct.borrow().clone();
                                            if !old_turn.is_empty() {
                                                let name = (speaker2_name)();
                                                // Cross-session force endpoint
                                                if let Some(ref sess) = *s1_for_force.borrow() {
                                                    let _ = sess.force_endpoint();
                                                }
                                                // Insert words into live zone at chrono positions
                                                let elapsed = ss2.get();
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
                                                ltv.set(ltv() + 1);
                                                if anthropic_configured() && (auto_format_enabled)()
                                                {
                                                    tcc.set(tcc.get() + 1);
                                                    if tcc.get().is_multiple_of((format_nth)()) {
                                                        npc.set(true);
                                                    }
                                                }
                                            }
                                        }

                                        if is_new_turn {
                                            let elapsed = js_sys::Date::now() - start_time;
                                            ss2.set(elapsed);
                                            ts2.set(elapsed);
                                        }

                                        cto.set(turn_order);
                                        *ct.borrow_mut() = text.clone();
                                        p2_sig.set(text);
                                        ff2.set(is_formatted);
                                    },
                                    {
                                        let mut rec_state = rec_state;
                                        let mut status_msg = status_msg;
                                        let mut error_msg = error_msg;
                                        move |code: u16, reason: String| {
                                            rec_state.set(RecState::Stopped);
                                            if code != 1000 {
                                                error_msg.set(Some(format!(
                                                    "Speaker 2 disconnected (code {code}): {reason}"
                                                )));
                                            }
                                            status_msg.set("Disconnected".into());
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
                    countdown_secs.set(Some(idle_minutes() * 60));
                    let last_activity = last_activity.clone();
                    let session_for_timeout = session1_rc.clone();
                    let cap_for_timeout = cap_rc;
                    let session2_for_timeout = if multi {
                        (session_handle2)().clone()
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
                    let ticker_needs_para = needs_para_check.clone();
                    let ticker_last_speaker = last_speaker.clone();
                    let ticker_live_turns = live_turns_rc.clone();
                    let ticker_ts1 = turn_start1.clone();
                    let ticker_ts2 = turn_start2.clone();
                    let mut ticker_ltv = live_turns_version;

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

                            // STT cost accumulation (runs every second while recording)
                            {
                                // $0.45/hr per session = $0.000125/sec
                                let rate_per_sec = if multi { 0.000125 * 2.0 } else { 0.000125 };
                                stt_cost.set(stt_cost() + rate_per_sec);
                            }

                            // Paragraph detection
                            if ticker_needs_para.get() {
                                ticker_needs_para.set(false);
                                let model = (format_model)();
                                let depth = (format_depth)();
                                let ctx_depth = (format_context_depth)();
                                let mut t = transcript_t;
                                let mut llm = llm_cost;
                                spawn(async move {
                                    let text = (t)();
                                    if !text.is_empty()
                                        && let Some(result) =
                                            check_formatting(&text, multi, &model, depth, ctx_depth)
                                                .await
                                    {
                                        // Accumulate LLM cost
                                        let model_val = (format_model)();
                                        let (in_rate, out_rate) = llm_rates(&model_val);
                                        llm.set(
                                            llm()
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

                            // Graduate live turns (multi-speaker only)
                            if multi {
                                let before = ticker_live_turns.borrow().len();
                                graduate_live_words(
                                    &ticker_live_turns,
                                    &mut transcript_t,
                                    &ticker_last_speaker,
                                    ticker_ts1.get(),
                                    ticker_ts2.get(),
                                    start_time,
                                    (show_labels)(),
                                    (show_timestamps)(),
                                );
                                let after = ticker_live_turns.borrow().len();
                                if before != after {
                                    ticker_ltv.set(ticker_ltv() + 1);
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
                                let p1 = (partial_t)();
                                if !p1.is_empty() {
                                    if multi {
                                        let elapsed = js_sys::Date::now() - start_time;
                                        let new_words =
                                            split_turn_to_words(elapsed, &(speaker1_name)(), &p1);
                                        let mut words = ticker_live_turns.borrow_mut();
                                        for w in new_words {
                                            let pos = words.partition_point(|x| {
                                                x.estimated_ms <= w.estimated_ms
                                            });
                                            words.insert(pos, w);
                                        }
                                    } else {
                                        let prev = (transcript_t)();
                                        let name = (speaker1_name)();
                                        let elapsed = js_sys::Date::now() - start_time;
                                        let new_text = build_committed_text(
                                            &prev,
                                            &p1,
                                            &name,
                                            false,
                                            (show_labels)(),
                                            (show_timestamps)(),
                                            elapsed,
                                            &ticker_last_speaker.borrow(),
                                        );
                                        transcript_t.set(new_text);
                                        *ticker_last_speaker.borrow_mut() = name;
                                    }
                                    partial_t.set(String::new());
                                }
                                if multi {
                                    let p2 = (partial2_t)();
                                    if !p2.is_empty() {
                                        let elapsed = js_sys::Date::now() - start_time;
                                        let new_words =
                                            split_turn_to_words(elapsed, &(speaker2_name)(), &p2);
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
                                        (show_labels)(),
                                        (show_timestamps)(),
                                    );
                                    ticker_ltv.set(ticker_ltv() + 1);
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
        if let Some(sess_rc) = (session_handle)().as_ref()
            && let Some(ref sess) = *sess_rc.borrow()
        {
            let _ = sess.force_endpoint();
        }
        let p = (partial)();
        if !p.is_empty() {
            let multi = (speaker2_enabled)();
            let name = (speaker1_name)();
            let start = (session_start_time)().unwrap_or(0.0);
            if multi {
                if let Some(ref live_rc) = (live_turns_shared)() {
                    let elapsed = js_sys::Date::now() - start;
                    let new_words = split_turn_to_words(elapsed, &name, &p);
                    let mut words = live_rc.borrow_mut();
                    for w in new_words {
                        let pos = words.partition_point(|x| x.estimated_ms <= w.estimated_ms);
                        words.insert(pos, w);
                    }
                    drop(words);
                    live_turns_version.set(live_turns_version() + 1);
                }
            } else {
                let prev = (transcript)();
                let ls = (last_committed_speaker)();
                let ls_name = ls.as_ref().map(|r| r.borrow().clone()).unwrap_or_default();
                let elapsed = js_sys::Date::now() - start;
                let new_text = build_committed_text(
                    &prev,
                    &p,
                    &name,
                    false,
                    (show_labels)(),
                    (show_timestamps)(),
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
        if let Some(ct) = (current_turn_shared)().as_ref() {
            *ct.borrow_mut() = String::new();
        }
        if let Some(cto) = (current_turn_order_shared)().as_ref() {
            cto.set(u32::MAX);
        }
    };

    // ── End Turn (speaker 2) ────────────────────────────────────────
    let on_end_turn2 = move |_| {
        if let Some(sess_rc) = (session_handle2)().as_ref()
            && let Some(ref sess) = *sess_rc.borrow()
        {
            let _ = sess.force_endpoint();
        }
        let p = (partial2)();
        if !p.is_empty() {
            let name = (speaker2_name)();
            let start = (session_start_time)().unwrap_or(0.0);
            if let Some(ref live_rc) = (live_turns_shared)() {
                let elapsed = js_sys::Date::now() - start;
                let new_words = split_turn_to_words(elapsed, &name, &p);
                let mut words = live_rc.borrow_mut();
                for w in new_words {
                    let pos = words.partition_point(|x| x.estimated_ms <= w.estimated_ms);
                    words.insert(pos, w);
                }
                drop(words);
                live_turns_version.set(live_turns_version() + 1);
            }
            partial2.set(String::new());
        }
        if let Some(ct) = (current_turn_shared2)().as_ref() {
            *ct.borrow_mut() = String::new();
        }
        if let Some(cto) = (current_turn_order_shared2)().as_ref() {
            cto.set(u32::MAX);
        }
    };

    // ── Stop ────────────────────────────────────────────────────────
    let on_stop = move |_| {
        // Immediately enter Stopping state — disables all buttons
        rec_state.set(RecState::Stopping);
        status_msg.set("Waiting for final transcript\u{2026}".into());

        spawn(async move {
            let multi = (speaker2_enabled)();

            // 1. Stop audio capture — no more audio sent to STT
            if let Some(cap_rc) = (capture_handle)().as_ref()
                && let Some(cap) = cap_rc.borrow_mut().take()
            {
                cap.stop();
            }
            if multi
                && let Some(cap_rc) = (capture_handle2)().as_ref()
                && let Some(cap) = cap_rc.borrow_mut().take()
            {
                cap.stop();
            }

            // 2. Force endpoint on active sessions and reset formatted flags
            let s1_has_partial = !(partial)().is_empty();
            let s2_has_partial = multi && !(partial2)().is_empty();

            if s1_has_partial && let Some(ref ff) = (turn_is_formatted1_shared)() {
                ff.set(false);
            }
            if s2_has_partial && let Some(ref ff) = (turn_is_formatted2_shared)() {
                ff.set(false);
            }

            if let Some(sess_rc) = (session_handle)().as_ref()
                && let Some(ref sess) = *sess_rc.borrow()
            {
                let _ = sess.force_endpoint();
            }
            if multi
                && let Some(sess_rc) = (session_handle2)().as_ref()
                && let Some(ref sess) = *sess_rc.borrow()
            {
                let _ = sess.force_endpoint();
            }

            // 3. Wait for formatted responses (5s safety timeout)
            let deadline = js_sys::Date::now() + 5_000.0;
            loop {
                let s1_done = !s1_has_partial
                    || (turn_is_formatted1_shared)()
                        .as_ref()
                        .map(|f| f.get())
                        .unwrap_or(true);
                let s2_done = !s2_has_partial
                    || (turn_is_formatted2_shared)()
                        .as_ref()
                        .map(|f| f.get())
                        .unwrap_or(true);
                if (s1_done && s2_done) || js_sys::Date::now() >= deadline {
                    break;
                }
                gloo_timers::future::TimeoutFuture::new(50).await;
            }

            // 4. Terminate sessions
            if let Some(sess_rc) = (session_handle)().as_ref()
                && let Some(ref sess) = *sess_rc.borrow()
            {
                let _ = sess.terminate();
            }
            if multi
                && let Some(sess_rc) = (session_handle2)().as_ref()
                && let Some(ref sess) = *sess_rc.borrow()
            {
                let _ = sess.terminate();
            }

            // 5. Flush speaker 1 partial
            let start = (session_start_time)().unwrap_or(0.0);
            let ls = (last_committed_speaker)();
            let p1 = (partial)();
            if !p1.is_empty() {
                if multi {
                    if let Some(ref live_rc) = (live_turns_shared)() {
                        let elapsed = js_sys::Date::now() - start;
                        let new_words = split_turn_to_words(elapsed, &(speaker1_name)(), &p1);
                        let mut words = live_rc.borrow_mut();
                        for w in new_words {
                            let pos = words.partition_point(|x| x.estimated_ms <= w.estimated_ms);
                            words.insert(pos, w);
                        }
                    }
                } else {
                    let ls_name = ls.as_ref().map(|r| r.borrow().clone()).unwrap_or_default();
                    let name = (speaker1_name)();
                    let prev = (transcript)();
                    let elapsed = js_sys::Date::now() - start;
                    let new_text = build_committed_text(
                        &prev,
                        &p1,
                        &name,
                        false,
                        (show_labels)(),
                        (show_timestamps)(),
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
                let p2 = (partial2)();
                if !p2.is_empty() {
                    if let Some(ref live_rc) = (live_turns_shared)() {
                        let elapsed = js_sys::Date::now() - start;
                        let new_words = split_turn_to_words(elapsed, &(speaker2_name)(), &p2);
                        let mut words = live_rc.borrow_mut();
                        for w in new_words {
                            let pos = words.partition_point(|x| x.estimated_ms <= w.estimated_ms);
                            words.insert(pos, w);
                        }
                    }
                    partial2.set(String::new());
                }
                // Force-graduate all remaining live words
                if let Some(ref live_rc) = (live_turns_shared)() {
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
                        (show_labels)(),
                        (show_timestamps)(),
                    );
                    live_turns_version.set(live_turns_version() + 1);
                }
            }
            // Run paragraph detection (gated by trigger strategy).
            // Authentication lives in the proxy; we always fire the
            // request and let it 412 if no key is configured.
            let do_auto_format = (auto_format_enabled)();
            let do_format_on_stop = (format_on_stop)();
            if do_auto_format {
                let model = (format_model)();
                let depth = (format_depth)();
                let ctx_depth = (format_context_depth)();
                let mut t = transcript;
                let mut llm = llm_cost;
                spawn(async move {
                    let text = (t)();
                    if !text.is_empty()
                        && let Some(result) =
                            check_formatting(&text, multi, &model, depth, ctx_depth).await
                    {
                        let (in_rate, out_rate) = llm_rates(&model);
                        llm.set(
                            llm()
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
                    // Full-transcript Sonnet pass on stop
                    if do_format_on_stop {
                        let sonnet = "claude-sonnet-4-6";
                        let text = (t)();
                        if !text.is_empty()
                            && let Some(result) = check_formatting(&text, multi, sonnet, 0, 0).await
                        {
                            let (in_rate, out_rate) = llm_rates(sonnet);
                            llm.set(
                                llm()
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
                // Auto-format disabled — skip incremental but still do the full Sonnet pass
                let mut t = transcript;
                let mut llm = llm_cost;
                spawn(async move {
                    let sonnet = "claude-sonnet-4-6";
                    let text = (t)();
                    if !text.is_empty()
                        && let Some(result) = check_formatting(&text, multi, sonnet, 0, 0).await
                    {
                        let (in_rate, out_rate) = llm_rates(sonnet);
                        llm.set(
                            llm()
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
        let text = (transcript)();
        if text.is_empty() {
            return;
        }
        let mode = (transfer_mode)();
        match mode {
            TransferMode::Copy => {
                if let Some(window) = web_sys::window() {
                    let clipboard = window.navigator().clipboard();
                    let _ = clipboard.write_text(&text);
                    transfer_feedback.set(Some("✓ Copied".into()));
                    spawn(async move {
                        gloo_timers::future::TimeoutFuture::new(2_000).await;
                        transfer_feedback.set(None);
                    });
                }
            }
            TransferMode::TxtFile => {
                let filename = if (prompt_filename)() {
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
                let fb = if (prompt_filename)() {
                    format!("✓ Saved as {filename}")
                } else {
                    "✓ Saved".into()
                };
                transfer_feedback.set(Some(fb));
                spawn(async move {
                    gloo_timers::future::TimeoutFuture::new(2_000).await;
                    transfer_feedback.set(None);
                });
            }
            TransferMode::MdFile => {
                let multi = (speaker2_enabled)();
                let duration_ms = (session_start_time)()
                    .map(|st| js_sys::Date::now() - st)
                    .unwrap_or(0.0);
                let md_content = if multi {
                    let s1 = (speaker1_name)();
                    let s2 = (speaker2_name)();
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
                let filename = if (prompt_filename)() {
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
                let fb = if (prompt_filename)() {
                    format!("✓ Saved as {filename}")
                } else {
                    "✓ Saved".into()
                };
                transfer_feedback.set(Some(fb));
                spawn(async move {
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
        transcript.set(String::new());
        partial.set(String::new());
        partial2.set(String::new());
        if let Some(ct) = (current_turn_shared)().as_ref() {
            *ct.borrow_mut() = String::new();
        }
        if let Some(cto) = (current_turn_order_shared)().as_ref() {
            cto.set(u32::MAX);
        }
        if let Some(ct) = (current_turn_shared2)().as_ref() {
            *ct.borrow_mut() = String::new();
        }
        if let Some(cto) = (current_turn_order_shared2)().as_ref() {
            cto.set(u32::MAX);
        }
        if let Some(npc) = (needs_paragraph_check_shared)().as_ref() {
            npc.set(false);
        }
        // Clear live zone
        if let Some(ref live_rc) = (live_turns_shared)() {
            live_rc.borrow_mut().clear();
            live_turns_version.set(live_turns_version() + 1);
        }
        // Reset cost counters
        stt_cost.set(0.0);
        llm_cost.set(0.0);
        if rec_state() == RecState::Stopped {
            rec_state.set(RecState::Idle);
            status_msg.set("Ready".into());
        }
    };

    // ── Reformat (on-demand, full transcript) ───────────────────────
    let on_reformat = move |_: Event<MouseData>| {
        let model = "claude-sonnet-4-6".to_string();
        let multi = (speaker2_enabled)();
        let mut t = transcript;
        let mut r = reformatting;
        let mut llm = llm_cost;
        spawn(async move {
            r.set(true);
            let text = (t)();
            if !text.is_empty()
                && let Some(result) = check_formatting(&text, multi, &model, 0, 0).await
            {
                let (in_rate, out_rate) = llm_rates(&model);
                llm.set(
                    llm()
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
                button {
                    class: "gear-btn",
                    title: "Settings",
                    onclick: move |_| show_settings.set(!show_settings()),
                    "\u{2699}\u{fe0f}"
                }
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
                            button {
                                class: "btn btn-endturn-inline",
                                onclick: on_end_turn1,
                                "\u{23ce} End Turn"
                            }
                        } else {
                            span { class: "current-turn-label current-turn-idle", "{s1_name_val}" }
                            p { class: "current-turn-text current-turn-placeholder",
                                "Not recording"
                            }
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
                            button {
                                class: "btn btn-endturn-inline",
                                onclick: on_end_turn2,
                                "\u{23ce} End Turn"
                            }
                        } else {
                            span { class: "current-turn-label current-turn-idle", "{s2_name_val}" }
                            p { class: "current-turn-text current-turn-placeholder",
                                "Not recording"
                            }
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
                if state == RecState::Idle || (state == RecState::Stopped && !has_text) {
                    button { class: "btn btn-record", onclick: on_record, "\u{25cf} Record" }
                }
                if state == RecState::Recording {
                    button { class: "btn btn-stop", onclick: on_stop, "\u{25a0} Stop" }
                    if !multi {
                        button { class: "btn btn-endturn", onclick: on_end_turn1, "\u{23ce} End Turn" }
                    }
                }
                if state == RecState::Stopping {
                    button { class: "btn btn-stop", disabled: true, "\u{25a0} Stop" }
                    if !multi {
                        button { class: "btn btn-endturn", disabled: true, "\u{23ce} End Turn" }
                    }
                }
                if state == RecState::Stopped && has_text {
                    button { class: "btn btn-continue", onclick: on_continue, "\u{25cf} Continue" }
                }
                // Transfer combo button
                if has_text {
                    div { class: "transfer-combo",
                        button {
                            class: "btn btn-transfer-main",
                            disabled: state == RecState::Stopping,
                            onclick: on_transfer,
                            if let Some(ref feedback) = fb {
                                "{feedback}"
                            } else {
                                "{tm.label()}"
                            }
                        }
                        button {
                            class: "btn btn-transfer-arrow",
                            onclick: move |_| show_transfer_menu.set(!(show_transfer_menu)()),
                            "\u{25be}"
                        }
                        if (show_transfer_menu)() {
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
                }
                if has_text {
                    button {
                        class: "btn btn-clear",
                        disabled: state == RecState::Stopping,
                        onclick: on_clear,
                        "Clear"
                    }
                }
                if anthropic_configured() {
                    div { class: "format-combo",
                        button {
                            class: "btn btn-reformat-main",
                            onclick: on_reformat,
                            disabled: !has_text || reformatting() || state == RecState::Stopping,
                            if reformatting() {
                                "Reformatting\u{2026}"
                            } else {
                                "\u{00b6} Reformat"
                            }
                        }
                        button {
                            class: "btn btn-reformat-arrow",
                            onclick: move |_| show_format_menu.set(!(show_format_menu)()),
                            "\u{25be}"
                        }
                        if (show_format_menu)() {
                            div {
                                class: "format-overlay",
                                onclick: move |_| show_format_menu.set(false),
                            }
                            div { class: "format-menu",
                                div { class: "format-menu-title", "Formatting Settings" }

                                label { r#for: "fmt-model", "Incremental auto-format model" }
                                select {
                                    id: "fmt-model",
                                    class: "settings-input",
                                    value: "{format_model}",
                                    onchange: move |evt: Event<FormData>| {
                                        let val = evt.value();
                                        format_model.set(val.clone());
                                        save("parley_format_model", &val);
                                    },
                                    option { value: "claude-haiku-4-5-20251001", "Haiku 4.5 (fast, cheap)" }
                                    option { value: "claude-sonnet-4-6", "Sonnet 4.6 (better)" }
                                }

                                label { class: "checkbox-label",
                                    input {
                                        r#type: "checkbox",
                                        checked: auto_format_enabled(),
                                        onchange: move |evt: Event<FormData>| {
                                            let val = evt.value() == "true";
                                            auto_format_enabled.set(val);
                                            save("parley_auto_format", if val { "true" } else { "false" });
                                        },
                                    }
                                    " Auto-format every N turns"
                                }

                                if auto_format_enabled() {
                                    label { r#for: "fmt-nth", "N (every Nth turn)" }
                                    input {
                                        id: "fmt-nth",
                                        r#type: "number",
                                        class: "settings-input",
                                        min: "1",
                                        max: "20",
                                        value: "{format_nth}",
                                        oninput: move |evt: Event<FormData>| {
                                            if let Ok(v) = evt.value().parse::<u32>() {
                                                let v = v.max(1);
                                                format_nth.set(v);
                                                save("parley_format_nth", &v.to_string());
                                            }
                                        },
                                    }
                                }

                                label { r#for: "fmt-depth", "Reformat depth (chunks)" }
                                input {
                                    id: "fmt-depth",
                                    r#type: "number",
                                    class: "settings-input",
                                    min: "1",
                                    max: "6",
                                    value: "{format_depth}",
                                    oninput: move |evt: Event<FormData>| {
                                        if let Ok(v) = evt.value().parse::<usize>() {
                                            let v = v.clamp(1, 6);
                                            format_depth.set(v);
                                            save("parley_format_depth", &v.to_string());
                                        }
                                    },
                                }

                                label { r#for: "fmt-ctx-depth", "Additional visibility depth (chunks)" }
                                input {
                                    id: "fmt-ctx-depth",
                                    r#type: "number",
                                    class: "settings-input",
                                    min: "1",
                                    max: "6",
                                    value: "{format_context_depth}",
                                    oninput: move |evt: Event<FormData>| {
                                        if let Ok(v) = evt.value().parse::<usize>() {
                                            let v = v.clamp(1, 6);
                                            format_context_depth.set(v);
                                            save("parley_format_context_depth", &v.to_string());
                                        }
                                    },
                                }

                                p { class: "settings-hint",
                                    "\u{00b6} Reformat always uses Sonnet 4.6 on the full transcript."
                                }

                                label { class: "checkbox-label",
                                    input {
                                        r#type: "checkbox",
                                        checked: format_on_stop(),
                                        onchange: move |evt: Event<FormData>| {
                                            let val = evt.value() == "true";
                                            format_on_stop.set(val);
                                            save("parley_format_on_stop", if val { "true" } else { "false" });
                                        },
                                    }
                                    " Also format on stop (full pass, Sonnet 4.6)"
                                }
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

            // ── Settings drawer ─────────────────────────────────────
            if show_settings() {
                div {
                    class: "settings-overlay",
                    onclick: move |_| show_settings.set(false),
                }
                div { class: "settings-drawer",
                    h2 { "Settings" }

                    // ── API keys (proxy-managed) ──────────────────────
                    // Keys live in the OS keystore via the proxy's
                    // `/api/secrets` surface. This panel manages the
                    // `default` credential for each provider; named
                    // credentials (multi-account) are managed via the
                    // proxy directly until v2 expands the UI.
                    h3 { class: "settings-section-heading", "API Keys" }
                    SecretsKeyRow {
                        provider: "assemblyai",
                        label: "AssemblyAI API Key",
                        hint: "Used for live transcription.",
                        configured: assemblyai_configured(),
                        on_changed: move |_| secrets_refresh.refresh(),
                    }
                    SecretsKeyRow {
                        provider: "anthropic",
                        label: "Anthropic API Key",
                        hint: "Used for paragraph detection and conversation mode.",
                        configured: anthropic_configured(),
                        on_changed: move |_| secrets_refresh.refresh(),
                    }
                    SecretsKeyRow {
                        provider: "elevenlabs",
                        label: "ElevenLabs API Key",
                        hint: "Used for spoken AI replies in Conversation Mode. Optional.",
                        configured: elevenlabs_configured(),
                        on_changed: move |_| secrets_refresh.refresh(),
                    }
                    SecretsKeyRow {
                        provider: "xai",
                        label: "xAI API Key",
                        hint: "Used for grok-stt transcription and grok-tts spoken replies. Optional.",
                        configured: xai_configured(),
                        on_changed: move |_| secrets_refresh.refresh(),
                    }

                    // ── Pipeline section ─────────────────────────────
                    h3 { class: "settings-section-heading", "Pipeline" }
                    p { class: "settings-hint",
                        "Choose which providers handle speech-to-text and text-to-speech. \
                         Both transcription mode and conversation mode read the same selection."
                    }

                    label { r#for: "pipeline-stt", "Speech-to-text" }
                    div { class: "settings-row",
                        crate::ui::stt_provider_picker::SttProviderPicker {
                            selected_provider_id: pipeline_stt_provider,
                        }
                    }
                    p { class: "settings-hint",
                        "Used for Conversation Mode capture. The standalone \
                         Transcription view is still AssemblyAI-only."
                    }

                    label { r#for: "pipeline-tts", "Text-to-speech" }
                    div { class: "settings-row",
                        select {
                            id: "pipeline-tts",
                            class: "settings-input",
                            value: "{pipeline_tts_provider}",
                            onchange: move |evt: Event<FormData>| {
                                let new_provider = evt.value();
                                // Voice ids are provider-specific; an
                                // ElevenLabs id passed to xAI is invalid,
                                // and vice versa. Clear the saved voice
                                // so the VoicePicker auto-picks the new
                                // provider's first voice once its catalog
                                // loads. Doing this in the change handler
                                // (instead of a use_effect) means a fresh
                                // page load doesn't wipe the saved pick.
                                if new_provider != *pipeline_tts_provider.peek() {
                                    pipeline_tts_voice.set(String::new());
                                }
                                pipeline_tts_provider.set(new_provider);
                            },
                            option { value: "elevenlabs", "ElevenLabs" }
                            option { value: "xai", "xAI (grok-tts)" }
                            option { value: "off", "Off (text only)" }
                        }
                    }
                    p { class: "settings-hint",
                        "Used in Conversation Mode for spoken replies. \
                         Requires the matching API key above (or set Off for text-only)."
                    }

                    if pipeline_tts_provider() != "off" {
                        label { r#for: "pipeline-voice", "Voice" }
                        div { class: "settings-row",
                            crate::ui::voice_picker::VoicePicker {
                                provider_id: ReadOnlySignal::from(pipeline_tts_provider),
                                credential: ReadOnlySignal::from(voice_credential),
                                selected_voice_id: pipeline_tts_voice,
                            }
                        }
                        p { class: "settings-hint",
                            "Voices come from the selected TTS provider's catalog."
                        }
                    }

                    // ── Language models (read-only info) ─────────────
                    h3 { class: "settings-section-heading", "Language models" }
                    p { class: "settings-hint",
                        "Formatting (paragraph detection) and conversation replies both use \
                         Anthropic today. Per-purpose model + provider selection is configured \
                         in your persona and model files under ~/.parley; the conversation \
                         view's Model dropdown lets you swap the active heavy model per session."
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

                    // ── Cost meter toggle ────────────────────────────
                    label { class: "option-row settings-option-row",
                        input {
                            r#type: "checkbox",
                            checked: "{show_cost_meter}",
                            onchange: move |evt: Event<FormData>| {
                                let v = evt.checked();
                                show_cost_meter.set(v);
                                save("parley_show_cost_meter", if v { "true" } else { "false" });
                            },
                        }
                        "Show cost meter"
                    }
                    p { class: "settings-hint",
                        "Display a running estimate of API costs (STT + LLM) in the status bar."
                    }

                    // ── Speakers section ────────────────────────────
                    div { class: "settings-section-header", "Speakers" }

                    // Speaker 1
                    div { class: "speaker-card",
                        div { class: "speaker-card-title", "Speaker 1 (You)" }
                        div { class: "speaker-card-row",
                            div { class: "speaker-field",
                                label { "Name" }
                                input {
                                    r#type: "text",
                                    class: "settings-input",
                                    value: "{speaker1_name}",
                                    oninput: move |evt: Event<FormData>| {
                                        let val = evt.value();
                                        speaker1_name.set(val.clone());
                                        save("parley_speaker1_name", &val);
                                    },
                                }
                            }
                            div { class: "speaker-field",
                                label { "Source" }
                                select {
                                    class: "settings-input",
                                    value: "{speaker1_source}",
                                    onchange: move |evt: Event<FormData>| {
                                        let val = evt.value();
                                        speaker1_source.set(val.clone());
                                        save("parley_speaker1_source", &val);
                                    },
                                    option { value: "mic", "Microphone" }
                                    option { value: "system", "System Audio" }
                                }
                            }
                        }
                    }

                    // Speaker 2
                    div { class: "speaker-card",
                        div { class: "speaker-card-title",
                            span { "Speaker 2 (Remote)" }
                            label { class: "toggle-switch",
                                input {
                                    r#type: "checkbox",
                                    checked: "{speaker2_enabled}",
                                    onchange: move |evt: Event<FormData>| {
                                        let v = evt.checked();
                                        speaker2_enabled.set(v);
                                        save("parley_speaker2_enabled", if v { "true" } else { "false" });
                                    },
                                }
                                span { class: "toggle-slider" }
                            }
                        }
                        if (speaker2_enabled)() {
                            div { class: "speaker-card-row",
                                div { class: "speaker-field",
                                    label { "Name" }
                                    input {
                                        r#type: "text",
                                        class: "settings-input",
                                        value: "{speaker2_name}",
                                        oninput: move |evt: Event<FormData>| {
                                            let val = evt.value();
                                            speaker2_name.set(val.clone());
                                            save("parley_speaker2_name", &val);
                                        },
                                    }
                                }
                                div { class: "speaker-field",
                                    label { "Source" }
                                    select {
                                        class: "settings-input",
                                        value: "{speaker2_source}",
                                        onchange: move |evt: Event<FormData>| {
                                            let val = evt.value();
                                            speaker2_source.set(val.clone());
                                            save("parley_speaker2_source", &val);
                                        },
                                        option { value: "mic", "Microphone" }
                                        option { value: "system", "System Audio" }
                                    }
                                }
                            }

                            // Options
                            div { class: "speaker-options",
                                label { class: "option-row",
                                    input {
                                        r#type: "checkbox",
                                        checked: "{show_labels}",
                                        onchange: move |evt: Event<FormData>| {
                                            let v = evt.checked();
                                            show_labels.set(v);
                                            save("parley_show_labels", if v { "true" } else { "false" });
                                        },
                                    }
                                    "Speaker labels ([Name] prefix)"
                                }
                                label { class: "option-row",
                                    input {
                                        r#type: "checkbox",
                                        checked: "{show_timestamps}",
                                        onchange: move |evt: Event<FormData>| {
                                            let v = evt.checked();
                                            show_timestamps.set(v);
                                            save("parley_show_timestamps", if v { "true" } else { "false" });
                                        },
                                    }
                                    "Timestamps ([MM:SS] prefix)"
                                }
                            }

                            p { class: "settings-hint settings-warn",
                                "\u{26a0} Two speakers = 2\u{00d7} AssemblyAI usage"
                            }
                        }
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
fn SecretsKeyRow(
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
        editing.set(!configured);
        if configured {
            value.set(String::new());
        }
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
        spawn(async move {
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
        spawn(async move {
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
.btn:active { transform: scale(0.97); }

.btn-record { background: #e94560; color: #fff; }
.btn-record:hover { background: #ff6b81; }
.btn-stop { background: #e94560; color: #fff; }
.btn-stop:hover { background: #ff6b81; }
.btn-continue { background: #4ecca3; color: #1a1a2e; }
.btn-continue:hover { background: #6ee6bb; }
.btn-endturn { background: #0f3460; color: #e0e0e0; }
.btn-endturn:hover { background: #1a4a7a; }
.btn-clear { background: transparent; color: #8888aa; border: 1px solid #8888aa; }
.btn-clear:hover { color: #e0e0e0; border-color: #e0e0e0; }
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
.btn-reformat-main:hover { color: #e0d0f0; border-color: #e0d0f0; }
.btn-reformat-main:disabled { opacity: 0.5; cursor: not-allowed; }
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
.btn-transfer-main:hover { background: #1a4a7a; }
.btn-transfer-arrow {
    background: #0f3460;
    color: #e0e0e0;
    border-radius: 0 8px 8px 0;
    padding: 0.65rem 0.5rem;
    border-left: 1px solid #1a1a2e;
    font-size: 0.85rem;
}
.btn-transfer-arrow:hover { background: #1a4a7a; }
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
