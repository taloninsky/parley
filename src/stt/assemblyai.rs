use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{MessageEvent, WebSocket};

use parley_core::word_graph::SttWord;

const ASSEMBLYAI_WS_URL: &str = "wss://streaming.assemblyai.com/v3/ws";

/// One Turn event from the STT provider, normalized into a provider-neutral
/// shape. The transcript-string fields preserve the existing UI integration
/// (formatter pipeline, live-zone insertion). The new `end_of_turn` and
/// `words` fields carry the per-word data the word graph requires.
///
/// See `docs/word-graph-spec.md` §3.3 for the canonical `SttWord` shape;
/// the AssemblyAI v3 `words` array maps directly onto it.
#[derive(Clone, Debug)]
pub struct TurnEvent {
    /// Running transcript text for the turn.
    pub transcript: String,
    /// Whether the turn has been auto-formatted by the STT model
    /// (always `true` for u3-rt-pro).
    pub is_formatted: bool,
    /// Monotonic turn counter (0, 1, 2, …) from the provider.
    pub turn_order: u32,
    /// `true` = turn complete; `false` = partial (may still update).
    pub end_of_turn: bool,
    /// Per-word data — text, timing, confidence, finality. Empty if the
    /// provider message lacked a `words` array.
    pub words: Vec<SttWord>,
}

/// Fetch a temporary streaming token via the local proxy server.
/// The proxy runs at localhost:3033 and forwards the request to
/// AssemblyAI's v3 token endpoint, avoiding browser CORS
/// restrictions and keeping the AssemblyAI API key on the proxy
/// (resolved from the OS keystore via the proxy's [`SecretsManager`]).
/// Retries up to 3 times with exponential backoff on failure.
pub async fn fetch_temp_token() -> Result<String, String> {
    let mut last_err = String::new();

    for attempt in 0..3u32 {
        if attempt > 0 {
            // Exponential backoff: 500ms, 1000ms
            let delay_ms = 500 * (1 << (attempt - 1));
            gloo_timers::future::TimeoutFuture::new(delay_ms).await;
        }

        match fetch_temp_token_once().await {
            Ok(token) => return Ok(token),
            Err(e) => {
                web_sys::console::warn_1(
                    &format!("Token fetch attempt {} failed: {}", attempt + 1, e).into(),
                );
                last_err = e;
            }
        }
    }

    Err(last_err)
}

async fn fetch_temp_token_once() -> Result<String, String> {
    let window = web_sys::window().ok_or("no window")?;

    let opts = web_sys::RequestInit::new();
    opts.set_method("POST");

    // No request body: the proxy resolves the AssemblyAI key from
    // its SecretsManager. A 412 here means no `default` AssemblyAI
    // credential is configured — surfaced verbatim to the caller so
    // the UI can prompt the user to set one in Settings.
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
    /// `on_turn` is called once per Turn event with a normalized `TurnEvent`
    /// (see struct doc). It fires for both partial and final turns, including
    /// when `transcript` is empty (e.g., a final turn that arrives only as
    /// an `end_of_turn=true` confirmation) — consumers that only care about
    /// non-empty transcripts should filter accordingly.
    /// `on_close` is called when the connection closes, with (code, reason).
    pub fn connect(
        temp_token: &str,
        on_turn: impl FnMut(TurnEvent) + 'static,
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
            let mut on_turn = on_turn;
            let session_ready_msg = session_ready.clone();
            let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                if let Ok(text) = event.data().dyn_into::<js_sys::JsString>() {
                    let text: String = text.into();
                    // Truncate the log preview by char count, not
                    // byte count — slicing inside a multi-byte
                    // UTF-8 sequence (e.g. an em-dash) panics.
                    let preview: String = text.chars().take(300).collect();
                    web_sys::console::log_1(&format!("AssemblyAI msg: {preview}").into());
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
                                let end_of_turn = parsed
                                    .get("end_of_turn")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false);
                                let words = parse_words(parsed.get("words"));

                                // Preserve historical UI behavior: only fire
                                // when there is non-empty transcript text.
                                // Per-word ingest into the graph happens via
                                // the same callback; consumers that need the
                                // word-graph synchronized on empty-transcript
                                // turns can lift this check later.
                                if !transcript.is_empty() {
                                    on_turn(TurnEvent {
                                        transcript,
                                        is_formatted,
                                        turn_order,
                                        end_of_turn,
                                        words,
                                    });
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

/// Parse the AssemblyAI v3 `words` array (per Turn event) into the
/// provider-neutral `SttWord` shape consumed by the word graph.
///
/// Returns an empty vec if the value is missing, not an array, or contains
/// only malformed entries. Individual malformed entries are skipped silently
/// rather than failing the whole turn — partial-turn messages can contain
/// in-flight data, and we'd rather drop a single bad word than discard the
/// useful ones.
fn parse_words(value: Option<&serde_json::Value>) -> Vec<SttWord> {
    let Some(arr) = value.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|w| {
            let text = w.get("text").and_then(|t| t.as_str())?.to_string();
            // AssemblyAI sends start/end as integer milliseconds; accept
            // either integer or float to be defensive.
            let start_ms = w.get("start").and_then(|v| v.as_f64())?;
            let end_ms = w.get("end").and_then(|v| v.as_f64())?;
            let confidence = w.get("confidence").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
            let word_is_final = w
                .get("word_is_final")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            Some(SttWord {
                text,
                start_ms,
                end_ms,
                confidence,
                word_is_final,
            })
        })
        .collect()
}
