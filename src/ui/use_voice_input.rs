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
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::spawn_local;

use crate::audio::capture::BrowserCapture;
use crate::stt::assemblyai::{AssemblyAiSession, TurnEvent, fetch_temp_token};
use crate::stt::xai_proxy::XaiProxySession;

/// Provider-neutral handle the hook owns. `LiveCapture` keeps one of
/// these alongside the mic capture; the wrapper exists so `start()`
/// and `stop()` don't need to branch on provider id every time they
/// touch the session.
enum SttSession {
    AssemblyAi(Rc<AssemblyAiSession>),
    Xai(Rc<XaiProxySession>),
}

impl SttSession {
    fn send_audio(&self, samples: &[f32]) -> Result<(), JsValue> {
        match self {
            SttSession::AssemblyAi(s) => s.send_audio(samples),
            SttSession::Xai(s) => s.send_audio(samples),
        }
    }

    fn force_endpoint(&self) -> Result<(), JsValue> {
        match self {
            SttSession::AssemblyAi(s) => s.force_endpoint(),
            SttSession::Xai(s) => s.force_endpoint(),
        }
    }

    fn terminate(&self) -> Result<(), JsValue> {
        match self {
            SttSession::AssemblyAi(s) => s.terminate(),
            SttSession::Xai(s) => s.terminate(),
        }
    }

    fn is_xai(&self) -> bool {
        matches!(self, SttSession::Xai(_))
    }
}

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
/// The session is held in [`SttSession`] (not the concrete provider
/// type) so the start/stop callbacks don't have to branch on which
/// provider is active.
struct LiveCapture {
    session: SttSession,
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
            // Provider selection lives in cookies so it survives across
            // sessions; both the conversation view (here) and the
            // settings drawer read/write the same key. Defaults match
            // the proxy's fallback chain (AssemblyAI for STT today).
            let provider = crate::ui::pipeline::stt_provider();

            // Both providers share the same on_turn / on_close
            // closures: the wire format normalizes to `TurnEvent` at
            // the session boundary, so this layer doesn't care which
            // one produced the event.
            let on_turn = move |evt: TurnEvent| {
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
            let on_close = move |_code: u16, _reason: String| {
                // Informational; lifecycle is driven by stop().
            };

            let session: SttSession = match provider.as_str() {
                "xai" => {
                    // xAI streams through the proxy's `/api/stt/stream`
                    // bridge — no token fetch needed, the proxy holds
                    // the API key. Sample rate must match
                    // `BrowserCapture` (16 kHz today).
                    match XaiProxySession::connect("xai", "default", 16_000, on_turn, on_close) {
                        Ok(s) => SttSession::Xai(Rc::new(s)),
                        Err(e) => {
                            state.set(VoiceState::Error(format!("xai ws: {e:?}")));
                            return;
                        }
                    }
                }
                _ => {
                    // AssemblyAI: browser opens the WS directly with a
                    // short-lived token from the proxy.
                    let token = match fetch_temp_token().await {
                        Ok(t) => t,
                        Err(e) => {
                            state.set(VoiceState::Error(format!("token: {e}")));
                            return;
                        }
                    };
                    match AssemblyAiSession::connect(&token, on_turn, on_close) {
                        Ok(s) => SttSession::AssemblyAi(Rc::new(s)),
                        Err(e) => {
                            state.set(VoiceState::Error(format!("ws: {e:?}")));
                            return;
                        }
                    }
                }
            };

            // Clone the session handle for the audio callback so its
            // lifetime is independent of the holder's clone (which
            // stop() consumes to issue force_endpoint/terminate).
            let session_for_audio = match &session {
                SttSession::AssemblyAi(s) => SttSession::AssemblyAi(s.clone()),
                SttSession::Xai(s) => SttSession::Xai(s.clone()),
            };

            let capture = match BrowserCapture::start(move |samples: Vec<f32>| {
                let _ = session_for_audio.send_audio(&samples);
            })
            .await
            {
                Ok(c) => c,
                Err(e) => {
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
        if live.session.is_xai() {
            // xAI has no separate ForceEndpoint command. Its flush
            // signal is `audio.done`, which must come after the last
            // audio frame. Stop the mic first so the browser cannot
            // enqueue more PCM after the proxy has closed the upstream
            // audio sink.
            let LiveCapture { session, capture } = live;
            capture.stop();
            let _ = session.force_endpoint();
            spawn_local(async move {
                gloo_timers::future::TimeoutFuture::new(2000).await;
                let _ = session.terminate();
                drop(session);
                state.set(VoiceState::Idle);
            });
            return;
        }

        let _ = live.session.force_endpoint();

        // Give AssemblyAI a brief window to flush the trailing final
        // turn (its endpoint is async; the WS round-trip back through
        // the on_turn callback still has to happen). 400ms matches
        // what the parent transcription view uses for the same purpose.
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
