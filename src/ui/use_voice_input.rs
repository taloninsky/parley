//! Push-to-talk voice input hook for Conversation Mode.
//!
//! Wraps provider STT sessions plus [`BrowserCapture`] plumbing in a small,
//! reactive Dioxus hook so the conversation view can drive a press-to-start /
//! press-to-end transcription loop without re-implementing WebSocket lifecycle.
//!
//! Spec: `docs/conversation-voice-slice-spec.md` §7.1.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use dioxus::prelude::*;
use parley_core::stt::{SttGraphUpdate, SttMarker, SttStreamEvent, TokenStreamNormalizer};
use parley_core::word_graph::SttWord;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::spawn_local;

use crate::audio::capture::BrowserCapture;
use crate::stt::assemblyai::{AssemblyAiSession, TurnEvent, fetch_temp_token};
use crate::stt::soniox::{
    SONIOX_CONTEXT_TEXT_STORAGE_KEY, SONIOX_LATENCY_MODE_COOKIE, SonioxConfig, SonioxLatencyMode,
    SonioxSession, fetch_temp_api_key,
};
use crate::stt::xai_proxy::XaiProxySession;

fn load_cookie(key: &str) -> Option<String> {
    crate::ui::app::load(key)
}

fn load_local(key: &str) -> Option<String> {
    web_sys::window()?
        .local_storage()
        .ok()??
        .get_item(key)
        .ok()?
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

/// Provider-neutral handle the hook owns.
enum SttSession {
    AssemblyAi(Rc<AssemblyAiSession>),
    Soniox {
        session: Rc<SonioxSession>,
        finalize_seen: Rc<Cell<bool>>,
        finalize_settle_ms: u32,
    },
    Xai(Rc<XaiProxySession>),
}

impl SttSession {
    fn clone_handle(&self) -> Self {
        match self {
            Self::AssemblyAi(session) => Self::AssemblyAi(session.clone()),
            Self::Soniox {
                session,
                finalize_seen,
                finalize_settle_ms,
            } => Self::Soniox {
                session: session.clone(),
                finalize_seen: finalize_seen.clone(),
                finalize_settle_ms: *finalize_settle_ms,
            },
            Self::Xai(session) => Self::Xai(session.clone()),
        }
    }

    fn send_audio(&self, samples: &[f32]) -> Result<(), JsValue> {
        match self {
            Self::AssemblyAi(session) => session.send_audio(samples),
            Self::Soniox { session, .. } => session.send_audio(samples),
            Self::Xai(session) => session.send_audio(samples),
        }
    }

    fn finalize(&self) -> Result<(), JsValue> {
        match self {
            Self::AssemblyAi(session) => session.force_endpoint(),
            Self::Soniox {
                session,
                finalize_seen,
                ..
            } => {
                finalize_seen.set(false);
                session.finalize()
            }
            Self::Xai(session) => session.force_endpoint(),
        }
    }

    fn terminate(&self) -> Result<(), JsValue> {
        match self {
            Self::AssemblyAi(session) => session.terminate(),
            Self::Soniox { session, .. } => session.finish(),
            Self::Xai(session) => session.terminate(),
        }
    }

    fn finalize_complete(&self) -> bool {
        match self {
            Self::Soniox { finalize_seen, .. } => finalize_seen.get(),
            _ => false,
        }
    }

    fn finalize_settle_ms(&self) -> u32 {
        match self {
            Self::Soniox {
                finalize_settle_ms, ..
            } => *finalize_settle_ms,
            _ => 0,
        }
    }

    fn is_soniox(&self) -> bool {
        matches!(self, Self::Soniox { .. })
    }

    fn is_xai(&self) -> bool {
        matches!(self, Self::Xai(_))
    }
}

/// Coarse lifecycle state for the voice input loop.
#[derive(Debug, Clone, PartialEq)]
pub enum VoiceState {
    /// No active capture. `start()` will open a session.
    Idle,
    /// Microphone open and provider streaming.
    Listening,
    /// `stop()` was called; waiting for the STT provider to flush.
    Finalizing,
    /// Last attempt failed. `start()` is callable again.
    Error(String),
}

/// Public handle returned by [`use_voice_input`].
#[derive(Clone, Copy)]
pub struct VoiceInputHandle {
    /// Most recent partial transcript from the active turn.
    pub interim_text: Signal<String>,
    /// Concatenated final-turn transcripts, joined with spaces.
    pub final_text: Signal<String>,
    /// Current lifecycle state.
    pub state: Signal<VoiceState>,
    /// Open the mic and start streaming.
    pub start: Callback<()>,
    /// Force-flush any in-flight transcript and tear down.
    pub stop: Callback<()>,
}

/// Internal, non-reactive holder for the live session plus capture.
struct LiveCapture {
    session: SttSession,
    capture: BrowserCapture,
}

/// Construct a voice input hook scoped to the calling component.
pub fn use_voice_input() -> VoiceInputHandle {
    let mut interim_text = use_signal(String::new);
    let mut final_text = use_signal(String::new);
    let mut state = use_signal(|| VoiceState::Idle);
    let holder: Rc<RefCell<Option<LiveCapture>>> = use_hook(|| Rc::new(RefCell::new(None)));

    let holder_for_start = holder.clone();
    let start = use_callback(move |_: ()| {
        if matches!(
            *state.peek(),
            VoiceState::Listening | VoiceState::Finalizing
        ) {
            return;
        }

        interim_text.set(String::new());
        final_text.set(String::new());
        state.set(VoiceState::Listening);

        let holder = holder_for_start.clone();
        spawn_local(async move {
            let provider = crate::ui::pipeline::stt_provider();
            let session = match provider.as_str() {
                "soniox" => start_soniox_session(state, interim_text, final_text).await,
                "xai" => start_xai_session(state, interim_text, final_text),
                _ => start_assemblyai_session(state, interim_text, final_text).await,
            };

            let Some(session) = session else {
                return;
            };

            let session_for_audio = session.clone_handle();
            let capture = match BrowserCapture::start(move |samples: Vec<f32>| {
                let _ = session_for_audio.send_audio(&samples);
            })
            .await
            {
                Ok(capture) => capture,
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

        let live = holder_for_stop.borrow_mut().take();
        let Some(live) = live else {
            state.set(VoiceState::Idle);
            return;
        };

        if live.session.is_xai() {
            let LiveCapture { session, capture } = live;
            capture.stop();
            let _ = session.finalize();
            spawn_local(async move {
                gloo_timers::future::TimeoutFuture::new(2000).await;
                let _ = session.terminate();
                drop(session);
                state.set(VoiceState::Idle);
            });
            return;
        }

        spawn_local(async move {
            let settle_ms = live.session.finalize_settle_ms();
            if settle_ms > 0 {
                gloo_timers::future::TimeoutFuture::new(settle_ms).await;
            }
            let _ = live.session.finalize();

            if live.session.is_soniox() {
                for _ in 0..30 {
                    if live.session.finalize_complete() {
                        break;
                    }
                    gloo_timers::future::TimeoutFuture::new(100).await;
                }
            } else {
                gloo_timers::future::TimeoutFuture::new(400).await;
            }

            let _ = live.session.terminate();
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

async fn start_assemblyai_session(
    mut state: Signal<VoiceState>,
    mut interim_text: Signal<String>,
    mut final_text: Signal<String>,
) -> Option<SttSession> {
    let token = match fetch_temp_token().await {
        Ok(token) => token,
        Err(e) => {
            state.set(VoiceState::Error(format!("token: {e}")));
            return None;
        }
    };

    let on_turn = move |evt: TurnEvent| {
        accept_turn_event(evt, &mut interim_text, &mut final_text);
    };
    let on_close = move |_code: u16, _reason: String| {};

    match AssemblyAiSession::connect(&token, on_turn, on_close) {
        Ok(session) => Some(SttSession::AssemblyAi(Rc::new(session))),
        Err(e) => {
            state.set(VoiceState::Error(format!("ws: {e:?}")));
            None
        }
    }
}

async fn start_soniox_session(
    mut state: Signal<VoiceState>,
    mut interim_text: Signal<String>,
    mut final_text: Signal<String>,
) -> Option<SttSession> {
    let latency_mode = selected_soniox_latency_mode();
    let context_text = selected_soniox_context_text();
    let token = match fetch_temp_api_key().await {
        Ok(token) => token,
        Err(e) => {
            state.set(VoiceState::Error(format!("soniox token: {e}")));
            return None;
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

    match SonioxSession::connect(
        &token,
        SonioxConfig::for_latency_mode_and_context(latency_mode, context_text),
        on_event,
        on_close,
    ) {
        Ok(session) => Some(SttSession::Soniox {
            session: Rc::new(session),
            finalize_seen,
            finalize_settle_ms: latency_mode.finalize_settle_ms(),
        }),
        Err(e) => {
            state.set(VoiceState::Error(format!("soniox ws: {e:?}")));
            None
        }
    }
}

fn start_xai_session(
    mut state: Signal<VoiceState>,
    mut interim_text: Signal<String>,
    mut final_text: Signal<String>,
) -> Option<SttSession> {
    let on_turn = move |evt: TurnEvent| {
        accept_turn_event(evt, &mut interim_text, &mut final_text);
    };
    let on_close = move |_code: u16, _reason: String| {};

    match XaiProxySession::connect("xai", "default", 16_000, on_turn, on_close) {
        Ok(session) => Some(SttSession::Xai(Rc::new(session))),
        Err(e) => {
            state.set(VoiceState::Error(format!("xai ws: {e:?}")));
            None
        }
    }
}

fn accept_turn_event(
    evt: TurnEvent,
    interim_text: &mut Signal<String>,
    final_text: &mut Signal<String>,
) {
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
}
