use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{MessageEvent, WebSocket};

const ASSEMBLYAI_WS_URL: &str = "wss://streaming.assemblyai.com/v3/ws";

/// Fetch a temporary streaming token via the local proxy server.
/// The proxy runs at localhost:3033 and forwards the request to AssemblyAI's
/// v3 token endpoint, avoiding browser CORS restrictions.
pub async fn fetch_temp_token(api_key: &str) -> Result<String, String> {
    let window = web_sys::window().ok_or("no window")?;

    let opts = web_sys::RequestInit::new();
    opts.set_method("POST");

    let headers = web_sys::Headers::new().map_err(|e| format!("{e:?}"))?;
    headers
        .set("Content-Type", "application/json")
        .map_err(|e| format!("{e:?}"))?;
    opts.set_headers(&headers);

    let body = format!(r#"{{"api_key":"{}"}}"#, api_key);
    opts.set_body(&JsValue::from_str(&body));

    let request = web_sys::Request::new_with_str_and_init("http://127.0.0.1:3033/token", &opts)
        .map_err(|e| format!("{e:?}"))?;

    let resp_value = wasm_bindgen_futures::JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|e| format!("proxy fetch failed: {e:?}"))?;

    let resp: web_sys::Response = resp_value
        .dyn_into()
        .map_err(|_| "response cast failed".to_string())?;

    if !resp.ok() {
        let status = resp.status();
        let text = wasm_bindgen_futures::JsFuture::from(resp.text().map_err(|e| format!("{e:?}"))?)
            .await
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default();
        return Err(format!("proxy returned HTTP {status}: {text}"));
    }

    let json = wasm_bindgen_futures::JsFuture::from(resp.json().map_err(|e| format!("{e:?}"))?)
        .await
        .map_err(|e| format!("json parse failed: {e:?}"))?;

    js_sys::Reflect::get(&json, &JsValue::from_str("token"))
        .map_err(|_| "no token field".to_string())?
        .as_string()
        .ok_or("token not a string".to_string())
}

/// Active streaming session with AssemblyAI v3 Universal Streaming.
pub struct AssemblyAiSession {
    ws: WebSocket,
    session_ready: Rc<Cell<bool>>,
}

impl AssemblyAiSession {
    /// Open a v3 Universal Streaming WebSocket.
    ///
    /// `on_transcript` is called with (text, is_formatted) for each Turn event.
    ///   is_formatted=true means the turn is finalized with punctuation.
    /// `on_close` is called when the connection closes, with (code, reason).
    pub fn connect(
        temp_token: &str,
        on_transcript: impl FnMut(String, bool, u32) + 'static,
        on_close: impl FnMut(u16, String) + 'static,
    ) -> Result<Self, JsValue> {
        let url = format!(
            "{}?speech_model=u3-rt-pro&sample_rate=16000&token={}",
            ASSEMBLYAI_WS_URL, temp_token
        );

        web_sys::console::log_1(
            &format!(
                "Connecting to: {}?speech_model=u3-rt-pro&sample_rate=16000&token=<redacted>",
                ASSEMBLYAI_WS_URL
            )
            .into(),
        );

        let ws = WebSocket::new(&url)?;
        ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

        let session_ready = Rc::new(Cell::new(false));

        // On open
        {
            let on_open = Closure::<dyn FnMut()>::new(move || {
                web_sys::console::log_1(&"AssemblyAI WebSocket connected".into());
            });
            ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
            on_open.forget();
        }

        // On message: parse v3 events (Begin, Turn, Termination)
        {
            let mut on_transcript = on_transcript;
            let session_ready_msg = session_ready.clone();
            let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                if let Ok(text) = event.data().dyn_into::<js_sys::JsString>() {
                    let text: String = text.into();
                    web_sys::console::log_1(
                        &format!("AssemblyAI msg: {}", &text[..text.len().min(300)]).into(),
                    );
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) {
                        match parsed.get("type").and_then(|t| t.as_str()) {
                            Some("Turn") => {
                                let transcript = parsed
                                    .get("transcript")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let is_formatted = parsed
                                    .get("turn_is_formatted")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false);
                                let turn_order = parsed
                                    .get("turn_order")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0)
                                    as u32;
                                if !transcript.is_empty() {
                                    on_transcript(transcript, is_formatted, turn_order);
                                }
                            }
                            Some("Begin") => {
                                web_sys::console::log_1(&"AssemblyAI session began (v3)".into());
                                session_ready_msg.set(true);
                            }
                            Some("Termination") => {
                                web_sys::console::log_1(
                                    &"AssemblyAI session terminated (v3)".into(),
                                );
                            }
                            _ => {}
                        }
                    }
                }
            });
            ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
            on_message.forget();
        }

        // On error (just log — close event always follows with the actual code)
        {
            let on_err =
                Closure::<dyn FnMut(web_sys::Event)>::new(move |_event: web_sys::Event| {
                    web_sys::console::error_1(&"AssemblyAI WebSocket error event fired".into());
                });
            ws.set_onerror(Some(on_err.as_ref().unchecked_ref()));
            on_err.forget();
        }

        // On close
        {
            let mut on_close = on_close;
            let on_cls = Closure::<dyn FnMut(web_sys::CloseEvent)>::new(
                move |event: web_sys::CloseEvent| {
                    let code = event.code();
                    let reason = event.reason();
                    web_sys::console::log_1(
                        &format!("AssemblyAI WebSocket closed: code={code}, reason={reason}")
                            .into(),
                    );
                    on_close(code, reason);
                },
            );
            ws.set_onclose(Some(on_cls.as_ref().unchecked_ref()));
            on_cls.forget();
        }

        Ok(Self { ws, session_ready })
    }

    /// Send raw PCM audio data (f32 samples) as binary PCM16 LE bytes.
    /// v3 accepts raw binary audio frames directly.
    pub fn send_audio(&self, samples: &[f32]) -> Result<(), JsValue> {
        if self.ws.ready_state() != WebSocket::OPEN || !self.session_ready.get() {
            return Ok(());
        }
        let mut buf = Vec::with_capacity(samples.len() * 2);
        for &sample in samples {
            let clamped = sample.clamp(-1.0, 1.0);
            let val = (clamped * 32767.0) as i16;
            buf.extend_from_slice(&val.to_le_bytes());
        }
        self.ws.send_with_u8_array(&buf)
    }

    /// Send a Terminate message to gracefully end the session.
    pub fn terminate(&self) -> Result<(), JsValue> {
        if self.ws.ready_state() == WebSocket::OPEN {
            self.ws.send_with_str(r#"{"type": "Terminate"}"#)?;
        }
        Ok(())
    }

    /// Force the current turn to end immediately.
    pub fn force_endpoint(&self) -> Result<(), JsValue> {
        if self.ws.ready_state() == WebSocket::OPEN {
            self.ws.send_with_str(r#"{"type": "ForceEndpoint"}"#)?;
        }
        Ok(())
    }
}
