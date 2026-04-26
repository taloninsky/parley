//! Browser-side WebSocket client for the proxy's `/api/stt/stream`
//! endpoint when the selected STT provider is xAI.
//!
//! Mirrors the public surface of [`crate::stt::assemblyai::AssemblyAiSession`]
//! so [`crate::ui::use_voice_input`] can dispatch between the two without
//! re-implementing the lifecycle.
//!
//! ## Protocol
//!
//! Per `docs/xai-speech-integration-spec.md` §8.2:
//!
//! - **Browser → Proxy** (binary): PCM16 LE audio frames at the configured
//!   sample rate.
//! - **Browser → Proxy** (text): `{"type":"audio.done"}` to close cleanly.
//! - **Proxy → Browser** (text): `parley_core::stt::TranscriptEvent` JSON
//!   (`{"kind":"partial","text":"…"}`, `{"kind":"final","text":"…"}`,
//!   `{"kind":"done","duration_seconds":n}`) or
//!   `{"type":"error","message":"…"}` on failure.
//!
//! ## Mapping to `TurnEvent`
//!
//! [`TranscriptEvent::Partial`] becomes a [`TurnEvent`] with
//! `end_of_turn=false`; [`TranscriptEvent::Final`] becomes one with
//! `end_of_turn=true`. xAI's stream doesn't carry per-word timing yet, so
//! the `words` field is always empty — consumers that need word-level
//! data (the live transcription view) won't get it from this provider
//! today.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use parley_core::stt::TranscriptEvent;
use parley_core::word_graph::SttWord;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{MessageEvent, WebSocket};

use crate::stt::assemblyai::TurnEvent;

const PROXY_STT_WS_URL: &str = "ws://127.0.0.1:3033/api/stt/stream";
const XAI_STT_DEFAULT_LANGUAGE: &str = "en";
const MAX_PENDING_AUDIO_CHUNKS: usize = 32;

/// Active streaming session bridged through the proxy to xAI.
pub struct XaiProxySession {
    ws: WebSocket,
    open: Rc<Cell<bool>>,
    audio_done_sent: Rc<Cell<bool>>,
    pending_audio: Rc<RefCell<Vec<Vec<u8>>>>,
    turn_counter: Rc<Cell<u32>>,
}

impl XaiProxySession {
    /// Connect to the proxy's STT bridge for `provider` / `credential`.
    /// `sample_rate_hz` must match what `BrowserCapture` is producing
    /// (16 kHz today).
    ///
    /// `on_turn` fires for every `Partial` / `Final` event the proxy
    /// forwards; `on_close` fires when the WS terminates (clean or
    /// otherwise) with the close code and reason.
    pub fn connect(
        provider: &str,
        credential: &str,
        sample_rate_hz: u32,
        on_turn: impl FnMut(TurnEvent) + 'static,
        on_close: impl FnMut(u16, String) + 'static,
    ) -> Result<Self, JsValue> {
        let url = format!(
            "{PROXY_STT_WS_URL}?provider={provider}&credential={credential}&sample_rate={sample_rate_hz}&language={XAI_STT_DEFAULT_LANGUAGE}",
        );
        web_sys::console::log_1(&format!("xAI STT: connecting to {url}").into());

        let ws = WebSocket::new(&url)?;
        ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

        let open = Rc::new(Cell::new(false));
        let audio_done_sent = Rc::new(Cell::new(false));
        let pending_audio = Rc::new(RefCell::new(Vec::<Vec<u8>>::new()));
        let turn_counter = Rc::new(Cell::new(0u32));

        {
            let open_oc = open.clone();
            let audio_done_oc = audio_done_sent.clone();
            let pending_audio_oc = pending_audio.clone();
            let ws_oc = ws.clone();
            let on_open = Closure::<dyn FnMut()>::new(move || {
                web_sys::console::log_1(&"xAI STT: WS connected".into());
                open_oc.set(true);
                for chunk in pending_audio_oc.borrow_mut().drain(..) {
                    let _ = ws_oc.send_with_u8_array(&chunk);
                }
                if audio_done_oc.get() {
                    let _ = ws_oc.send_with_str(r#"{"type":"audio.done"}"#);
                }
            });
            ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
            on_open.forget();
        }

        {
            let mut on_turn = on_turn;
            let counter = turn_counter.clone();
            let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                let Ok(text) = event.data().dyn_into::<js_sys::JsString>() else {
                    return;
                };
                let text: String = text.into();
                let preview: String = text.chars().take(300).collect();
                web_sys::console::log_1(&format!("xAI STT msg: {preview}").into());

                // The proxy multiplexes two distinct envelopes on the
                // text channel: canonical TranscriptEvent JSON (tagged
                // with `kind`) and error frames (tagged with `type`).
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text)
                    && parsed.get("type").and_then(|v| v.as_str()) == Some("error")
                {
                    let msg = parsed
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(no message)");
                    web_sys::console::error_1(&format!("xAI STT error: {msg}").into());
                    return;
                }

                match serde_json::from_str::<TranscriptEvent>(&text) {
                    Ok(TranscriptEvent::Partial { text }) => {
                        if text.trim().is_empty() {
                            return;
                        }
                        on_turn(TurnEvent {
                            transcript: text,
                            is_formatted: false,
                            turn_order: counter.get(),
                            end_of_turn: false,
                            words: Vec::<SttWord>::new(),
                        });
                    }
                    Ok(TranscriptEvent::Final { text, .. }) => {
                        if text.trim().is_empty() {
                            return;
                        }
                        let order = counter.get();
                        counter.set(order + 1);
                        on_turn(TurnEvent {
                            transcript: text,
                            is_formatted: false,
                            turn_order: order,
                            end_of_turn: true,
                            words: Vec::<SttWord>::new(),
                        });
                    }
                    Ok(TranscriptEvent::Done { .. }) => {
                        web_sys::console::log_1(&"xAI STT: session done".into());
                    }
                    Err(e) => {
                        web_sys::console::warn_1(
                            &format!("xAI STT: unparseable frame: {e}").into(),
                        );
                    }
                }
            });
            ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
            on_message.forget();
        }

        {
            let on_err = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                web_sys::console::error_1(&"xAI STT: WS error event".into());
            });
            ws.set_onerror(Some(on_err.as_ref().unchecked_ref()));
            on_err.forget();
        }

        {
            let mut on_close = on_close;
            let open_oc = open.clone();
            let on_cls = Closure::<dyn FnMut(web_sys::CloseEvent)>::new(
                move |event: web_sys::CloseEvent| {
                    open_oc.set(false);
                    let code = event.code();
                    let reason = event.reason();
                    web_sys::console::log_1(
                        &format!("xAI STT: WS closed code={code} reason={reason}").into(),
                    );
                    on_close(code, reason);
                },
            );
            ws.set_onclose(Some(on_cls.as_ref().unchecked_ref()));
            on_cls.forget();
        }

        Ok(Self {
            ws,
            open,
            audio_done_sent,
            pending_audio,
            turn_counter,
        })
    }

    /// Send PCM samples as binary PCM16 LE bytes. Startup frames are
    /// buffered until the proxy WS opens so short utterances do not
    /// lose their first syllable while the handshake is completing.
    pub fn send_audio(&self, samples: &[f32]) -> Result<(), JsValue> {
        if self.audio_done_sent.get() {
            return Ok(());
        }

        let buf = samples_to_pcm16_le(samples);
        if !self.open.get() || self.ws.ready_state() != WebSocket::OPEN {
            let mut pending = self.pending_audio.borrow_mut();
            if pending.len() >= MAX_PENDING_AUDIO_CHUNKS {
                pending.remove(0);
            }
            pending.push(buf);
            return Ok(());
        }
        self.ws.send_with_u8_array(&buf)
    }

    /// Signal end-of-input. Sends `{"type":"audio.done"}` so the proxy
    /// closes the audio sink cleanly (which lets xAI flush any pending
    /// final transcript before terminating).
    pub fn terminate(&self) -> Result<(), JsValue> {
        if self.audio_done_sent.replace(true) {
            return Ok(());
        }
        if self.ws.ready_state() == WebSocket::OPEN {
            self.ws.send_with_str(r#"{"type":"audio.done"}"#)?;
        }
        Ok(())
    }

    /// Best-effort early flush. xAI doesn't expose a `ForceEndpoint`
    /// equivalent today, so this is the same as [`Self::terminate`] —
    /// closing the audio sink is the only way to make the provider emit
    /// any pending final.
    pub fn force_endpoint(&self) -> Result<(), JsValue> {
        self.terminate()
    }

    /// `true` once the WS handshake completed.
    #[allow(dead_code)]
    pub fn is_open(&self) -> bool {
        self.open.get()
    }

    /// Current turn counter (monotonic across finals). Exposed mostly
    /// for tests; production code reads `TurnEvent::turn_order` instead.
    #[allow(dead_code)]
    pub fn turn_count(&self) -> u32 {
        self.turn_counter.get()
    }
}

fn samples_to_pcm16_le(samples: &[f32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(samples.len() * 2);
    for &sample in samples {
        let val = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
        buf.extend_from_slice(&val.to_le_bytes());
    }
    buf
}
