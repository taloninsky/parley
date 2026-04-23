//! Push-to-talk voice input hook for Conversation Mode.
//!
//! Wraps the existing [`AssemblyAiSession`] + [`BrowserCapture`]
//! plumbing in a small, reactive Dioxus hook so the conversation
//! view can drive a press-to-start / press-to-end transcription
//! loop without re-implementing the WebSocket lifecycle.
//!
//! Spec: `docs/conversation-voice-slice-spec.md` §7.1.
//!
//! ## Lifecycle
//!
//! 1. `start()` fetches a temporary AssemblyAI token from the
//!    proxy, opens a WebSocket session, and starts the browser's
//!    microphone capture. Audio frames flow into the WS as PCM16.
//! 2. AssemblyAI streams `Turn` events. Partial turns update
//!    [`VoiceInputHandle::interim_text`]; final turns
//!    (`end_of_turn=true`) append to
//!    [`VoiceInputHandle::final_text`].
//! 3. `stop()` issues a `ForceEndpoint` so AssemblyAI flushes any
//!    in-flight transcript as a final turn, then schedules a
//!    short timer and tears down the session. The hook
//!    transitions through `Finalizing` → `Idle`; consumers watch
//!    that transition to know when `final_text` is safe to
//!    submit.
//!
//! ## Why a fresh hook (not extracting from `src/ui/app.rs`)
//!
//! `app.rs` has an in-place transcription view with a deeply
//! interwoven set of signals (formatting pipeline, cursor
//! restoration, word graph). Extracting that machinery into a
//! shared hook is a separate refactor; this slice ships a focused
//! voice-input surface that re-uses the *underlying* primitives
//! (`AssemblyAiSession`, `BrowserCapture`, `fetch_temp_token`) but
//! owns its own session for the conversation view.

use std::cell::RefCell;
use std::rc::Rc;

use dioxus::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::audio::capture::BrowserCapture;
use crate::stt::assemblyai::{AssemblyAiSession, fetch_temp_token};

/// Coarse lifecycle state for the voice input loop.
#[derive(Debug, Clone, PartialEq)]
pub enum VoiceState {
    /// No active capture. `start()` will open a session.
    Idle,
    /// Microphone open and AssemblyAI streaming.
    Listening,
    /// `stop()` was called; waiting for AssemblyAI to flush the
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
    /// Accumulates across multiple AssemblyAI turns within a
    /// single listening session.
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
/// The session is wrapped in `Rc` because the `BrowserCapture`
/// audio callback needs its own clone to forward PCM frames; the
/// holder's clone is what `stop()` uses to call
/// `force_endpoint` / `terminate`.
struct LiveCapture {
    session: Rc<AssemblyAiSession>,
    capture: BrowserCapture,
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
            let token = match fetch_temp_token().await {
                Ok(t) => t,
                Err(e) => {
                    state.set(VoiceState::Error(format!("token: {e}")));
                    return;
                }
            };

            // The on_turn closure runs on every AssemblyAI Turn
            // event. We re-capture `interim_text` / `final_text`
            // (Copy signals) so the closure is `'static`.
            let on_turn = move |evt: crate::stt::assemblyai::TurnEvent| {
                if evt.end_of_turn {
                    // Append to the running final string with a
                    // space separator. The interim line clears
                    // because AssemblyAI is about to start a fresh
                    // turn.
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
            let on_close = move |_code: u16, _reason: String| {
                // Connection closed (either clean termination or
                // network error). The hook's lifecycle is driven
                // by the explicit `stop()` flow; this callback is
                // informational only.
            };

            let session = match AssemblyAiSession::connect(&token, on_turn, on_close) {
                Ok(s) => s,
                Err(e) => {
                    state.set(VoiceState::Error(format!("ws: {e:?}")));
                    return;
                }
            };
            let session = Rc::new(session);
            let session_for_audio = session.clone();

            let capture = match BrowserCapture::start(move |samples: Vec<f32>| {
                let _ = session_for_audio.send_audio(&samples);
            })
            .await
            {
                Ok(c) => c,
                Err(e) => {
                    // Tear down the WS we already opened so the
                    // mic-permission failure doesn't leave a
                    // zombie session.
                    let _ = session.terminate();
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
        let _ = live.session.force_endpoint();

        // Give AssemblyAI a brief window to flush the trailing
        // final turn (its endpoint is async; the WS round-trip
        // back through the on_turn callback still has to happen).
        // 400ms matches what the parent transcription view uses
        // for the same purpose.
        spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(400).await;
            let _ = live.session.terminate();
            // `BrowserCapture::stop` consumes self — closes the
            // mic and releases the device. Dropping our session
            // clone right after lets the WS close cleanly once
            // the capture callback's clone goes too.
            live.capture.stop();
            drop(live.session);
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
