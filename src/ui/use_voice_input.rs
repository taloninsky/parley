//! Push-to-talk voice input hook for Conversation Mode.
//!
//! Wraps provider STT sessions + [`BrowserCapture`] plumbing in a
//! small, reactive Dioxus hook so the conversation view can drive a
//! press-to-start / press-to-end transcription loop without
//! re-implementing the WebSocket lifecycle.
//!
//! Spec: `docs/conversation-voice-slice-spec.md` §7.1.
//!
//! ## Lifecycle
//!
//! 1. `start()` fetches a temporary token/key from the proxy, opens
//!    the selected provider WebSocket session, and starts browser
//!    microphone capture. Audio frames flow into the WS as PCM16.
//! 2. Provider partials update [`VoiceInputHandle::interim_text`];
//!    finalized text appends to [`VoiceInputHandle::final_text`].
//! 3. `stop()` asks the provider to finalize in-flight audio, then
//!    schedules a short teardown. The hook transitions through
//!    `Finalizing` → `Idle`; consumers watch that transition to know
//!    when `final_text` is safe to submit.
//!
//! ## Why a fresh hook (not extracting from `src/ui/app.rs`)
//!
//! `app.rs` has an in-place transcription view with a deeply
//! interwoven set of signals (formatting pipeline, cursor
//! restoration, word graph). Extracting that machinery into a
//! shared hook is a separate refactor; this slice ships a focused
//! voice-input surface that re-uses the *underlying* primitives
//! (`AssemblyAiSession`, `SonioxSession`, `BrowserCapture`) but owns
//! its own session for the conversation view.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use dioxus::prelude::*;
use parley_core::stt::{SttGraphUpdate, SttMarker, SttStreamEvent, TokenStreamNormalizer};
use parley_core::word_graph::SttWord;
use wasm_bindgen_futures::spawn_local;

use crate::audio::capture::BrowserCapture;
use crate::stt::assemblyai::{AssemblyAiSession, fetch_temp_token};
use crate::stt::soniox::{
    SONIOX_CONTEXT_TEXT_STORAGE_KEY, SONIOX_LATENCY_MODE_COOKIE, SonioxConfig, SonioxLatencyMode,
    SonioxSession, fetch_temp_api_key,
};

fn load_cookie(key: &str) -> Option<String> {
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

fn load_local(key: &str) -> Option<String> {
    web_sys::window()?
        .local_storage()
        .ok()??
        .get_item(key)
        .ok()?
}

fn selected_stt_provider() -> String {
    load_cookie("parley_stt_provider").unwrap_or_else(|| "assemblyai".to_string())
}

fn selected_soniox_latency_mode() -> SonioxLatencyMode {
    load_cookie(SONIOX_LATENCY_MODE_COOKIE)
        .as_deref()
        .and_then(SonioxLatencyMode::from_storage_value)
        .unwrap_or_default()
}

fn selected_soniox_context_text() -> Option<String> {
    load_local(SONIOX_CONTEXT_TEXT_STORAGE_KEY)
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn render_words(words: &[SttWord]) -> String {
    let mut out = String::new();
    for word in words {
        let is_punctuation = matches!(word.text.as_str(), "." | "," | "?" | "!" | ";" | ":" | "\"");
        if !out.is_empty() && !is_punctuation {
            out.push(' ');
        }
        out.push_str(&word.text);
    }
    out
}

fn render_update_words(update: &SttGraphUpdate, words: &[SttWord]) -> String {
    let text = render_words(words);
    if text.is_empty() {
        String::new()
    } else if update.lane == 0 {
        text
    } else {
        format!("[Speaker {}] {text}", update.lane + 1)
    }
}

fn render_updates(updates: &[SttGraphUpdate], provisional: bool) -> String {
    updates
        .iter()
        .filter_map(|update| {
            let words = if provisional {
                &update.provisional
            } else {
                &update.finalized
            };
            let text = render_update_words(update, words);
            (!text.is_empty()).then_some(text)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Coarse lifecycle state for the voice input loop.
#[derive(Debug, Clone, PartialEq)]
pub enum VoiceState {
    /// No active capture. `start()` will open a session.
    Idle,
    /// Microphone open and provider streaming.
    Listening,
    /// `stop()` was called; waiting for the STT provider to flush the
    /// trailing final turn before transitioning to `Idle`.
    Finalizing,
    /// Last attempt failed. `start()` is callable again.
    Error(String),
}

/// Public handle returned by [`use_voice_input`]. Cheap to clone —
/// only `Copy` signals + a `Callback` are exposed.
#[derive(Clone, Copy)]
pub struct VoiceInputHandle {
    /// Most recent partial transcript from the active turn.
    /// Cleared on every `start()` and on every transition to a
    /// final turn.
    pub interim_text: Signal<String>,
    /// Concatenated final-turn transcripts, joined with spaces.
    /// Accumulates across multiple finalized chunks within a single
    /// listening session.
    pub final_text: Signal<String>,
    /// Current lifecycle state.
    pub state: Signal<VoiceState>,
    /// Open the mic and start streaming.
    pub start: Callback<()>,
    /// Force-flush any in-flight transcript and tear down. After
    /// the hook transitions to `Idle`, `final_text` holds the
    /// caller's submittable string.
    pub stop: Callback<()>,
}

/// Internal, non-reactive holder for the live session + capture.
/// Stored in a `use_hook` slot so it survives renders without
/// being a `Signal` (web-sys handles aren't `Send` and we
/// don't want them flowing through reactivity).
///
/// The session is wrapped in `Rc` because the `BrowserCapture` audio
/// callback needs its own clone to forward PCM frames; the holder's
/// clone is what `stop()` uses to call provider finalization/teardown.
struct LiveCapture {
    session: Rc<LiveSession>,
    capture: BrowserCapture,
}

enum LiveSession {
    AssemblyAi(Rc<AssemblyAiSession>),
    Soniox {
        session: Rc<SonioxSession>,
        finalize_seen: Rc<Cell<bool>>,
        finalize_settle_ms: u32,
    },
}

impl LiveSession {
    fn send_audio(&self, samples: &[f32]) {
        match self {
            LiveSession::AssemblyAi(session) => {
                let _ = session.send_audio(samples);
            }
            LiveSession::Soniox { session, .. } => {
                let _ = session.send_audio(samples);
            }
        }
    }

    fn finalize(&self) {
        match self {
            LiveSession::AssemblyAi(session) => {
                let _ = session.force_endpoint();
            }
            LiveSession::Soniox {
                session,
                finalize_seen,
                ..
            } => {
                finalize_seen.set(false);
                let _ = session.finalize();
            }
        }
    }

    fn terminate(&self) {
        match self {
            LiveSession::AssemblyAi(session) => {
                let _ = session.terminate();
            }
            LiveSession::Soniox { session, .. } => {
                let _ = session.finish();
            }
        }
    }

    fn finalize_complete(&self) -> bool {
        match self {
            LiveSession::AssemblyAi(_) => false,
            LiveSession::Soniox { finalize_seen, .. } => finalize_seen.get(),
        }
    }

    fn finalize_settle_ms(&self) -> u32 {
        match self {
            LiveSession::AssemblyAi(_) => 0,
            LiveSession::Soniox {
                finalize_settle_ms, ..
            } => *finalize_settle_ms,
        }
    }

    fn is_soniox(&self) -> bool {
        matches!(self, LiveSession::Soniox { .. })
    }
}

/// Construct a voice input hook scoped to the calling component.
/// Calling this multiple times in the same component is supported
/// but each call gets its own independent session.
pub fn use_voice_input() -> VoiceInputHandle {
    let mut interim_text = use_signal(String::new);
    let mut final_text = use_signal(String::new);
    let mut state = use_signal(|| VoiceState::Idle);
    // Holder for the underlying handles. Wrapped in
    // `Rc<RefCell<...>>` so the start/stop callbacks (which are
    // `Copy` closures via `Callback`) can mutate the slot
    // without needing `Send`-able storage.
    let holder: Rc<RefCell<Option<LiveCapture>>> = use_hook(|| Rc::new(RefCell::new(None)));

    let holder_for_start = holder.clone();
    let start = use_callback(move |_: ()| {
        // Idempotent: clicking Start while already listening is a
        // no-op so an auto-listen retrigger doesn't double-open
        // the mic.
        if matches!(
            *state.peek(),
            VoiceState::Listening | VoiceState::Finalizing
        ) {
            return;
        }
        // Reset transcripts on every fresh listen. The previous
        // turn's text was already collapsed and submitted by the
        // caller (or discarded by mode flip).
        interim_text.set(String::new());
        final_text.set(String::new());
        state.set(VoiceState::Listening);

        let holder = holder_for_start.clone();
        spawn_local(async move {
            let provider = selected_stt_provider();
            let session = if provider == "soniox" {
                let latency_mode = selected_soniox_latency_mode();
                let context_text = selected_soniox_context_text();
                let token = match fetch_temp_api_key().await {
                    Ok(t) => t,
                    Err(e) => {
                        state.set(VoiceState::Error(format!("soniox token: {e}")));
                        return;
                    }
                };
                let normalizer = Rc::new(RefCell::new(TokenStreamNormalizer::new()));
                let finalize_seen = Rc::new(Cell::new(false));
                let on_event = {
                    let normalizer = normalizer.clone();
                    let finalize_seen = finalize_seen.clone();
                    move |event: SttStreamEvent| {
                        if let SttStreamEvent::Error { message, .. } = &event {
                            state.set(VoiceState::Error(format!("soniox: {message}")));
                            return;
                        }
                        let batch = match normalizer.borrow_mut().accept_event(event) {
                            Ok(batch) => batch,
                            Err(e) => {
                                state.set(VoiceState::Error(format!("soniox normalize: {e}")));
                                return;
                            }
                        };
                        let finalized = render_updates(&batch.updates, false);
                        if !finalized.is_empty() {
                            final_text.with_mut(|s| {
                                if !s.is_empty() {
                                    s.push(' ');
                                }
                                s.push_str(finalized.trim());
                            });
                        }
                        let provisional = render_updates(&batch.updates, true);
                        interim_text.set(provisional);
                        if batch.markers.contains(&SttMarker::FinalizeComplete) {
                            finalize_seen.set(true);
                            interim_text.set(String::new());
                        }
                    }
                };
                let on_close = move |_code: u16, _reason: String| {};
                let session = match SonioxSession::connect(
                    &token,
                    SonioxConfig::for_latency_mode_and_context(latency_mode, context_text),
                    on_event,
                    on_close,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        state.set(VoiceState::Error(format!("soniox ws: {e:?}")));
                        return;
                    }
                };
                LiveSession::Soniox {
                    session: Rc::new(session),
                    finalize_seen,
                    finalize_settle_ms: latency_mode.finalize_settle_ms(),
                }
            } else {
                let token = match fetch_temp_token().await {
                    Ok(t) => t,
                    Err(e) => {
                        state.set(VoiceState::Error(format!("token: {e}")));
                        return;
                    }
                };

                let on_turn = move |evt: crate::stt::assemblyai::TurnEvent| {
                    if evt.end_of_turn {
                        final_text.with_mut(|s| {
                            if !s.is_empty() {
                                s.push(' ');
                            }
                            s.push_str(evt.transcript.trim());
                        });
                        interim_text.set(String::new());
                    } else {
                        interim_text.set(evt.transcript);
                    }
                };
                let on_close = move |_code: u16, _reason: String| {};

                let session = match AssemblyAiSession::connect(&token, on_turn, on_close) {
                    Ok(s) => s,
                    Err(e) => {
                        state.set(VoiceState::Error(format!("ws: {e:?}")));
                        return;
                    }
                };
                LiveSession::AssemblyAi(Rc::new(session))
            };
            let session = Rc::new(session);
            let session_for_audio = session.clone();

            let capture = match BrowserCapture::start(move |samples: Vec<f32>| {
                session_for_audio.send_audio(&samples);
            })
            .await
            {
                Ok(c) => c,
                Err(e) => {
                    // Tear down the WS we already opened so the
                    // mic-permission failure doesn't leave a
                    // zombie session.
                    session.terminate();
                    state.set(VoiceState::Error(format!("mic: {e:?}")));
                    return;
                }
            };

            *holder.borrow_mut() = Some(LiveCapture { session, capture });
        });
    });

    let holder_for_stop = holder.clone();
    let stop = use_callback(move |_: ()| {
        if !matches!(*state.peek(), VoiceState::Listening) {
            return;
        }
        state.set(VoiceState::Finalizing);

        // Pull the live capture out of the holder so we can drop
        // it (which closes the mic) and force AssemblyAI to flush
        // the trailing turn.
        let live = holder_for_stop.borrow_mut().take();
        let Some(live) = live else {
            state.set(VoiceState::Idle);
            return;
        };
        spawn_local(async move {
            let settle_ms = live.session.finalize_settle_ms();
            if settle_ms > 0 {
                gloo_timers::future::TimeoutFuture::new(settle_ms).await;
            }
            live.session.finalize();

            if live.session.is_soniox() {
                // Soniox explicitly marks manual finalization with
                // `<fin>`. Wait for that marker so Conversation Mode
                // submits the final text, not a race against a fixed delay.
                for _ in 0..30 {
                    if live.session.finalize_complete() {
                        break;
                    }
                    gloo_timers::future::TimeoutFuture::new(100).await;
                }
            } else {
                // AssemblyAI ForceEndpoint has no marker in our current
                // adapter; preserve the existing short flush window.
                gloo_timers::future::TimeoutFuture::new(400).await;
            }
            live.session.terminate();
            // `BrowserCapture::stop` consumes self — closes the
            // mic and releases the device. Dropping our session
            // clone right after lets the WS close cleanly once
            // the capture callback's clone goes too.
            live.capture.stop();
            state.set(VoiceState::Idle);
        });
    });

    VoiceInputHandle {
        interim_text,
        final_text,
        state,
        start,
        stop,
    }
}
