use std::cell::Cell;
use std::rc::Rc;

use parley_core::stt::{SttMarker, SttStreamEvent, SttToken};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{MessageEvent, WebSocket};

const SONIOX_WS_URL: &str = "wss://stt-rt.soniox.com/transcribe-websocket";

pub const SONIOX_LATENCY_MODE_COOKIE: &str = "parley_soniox_latency_mode";
pub const SONIOX_CONTEXT_TEXT_STORAGE_KEY: &str = "parley_soniox_context_text";

/// Soniox-specific latency/accuracy tuning preset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SonioxLatencyMode {
    /// Prefer lower latency for quick turn-taking.
    Fast,
    /// Balance endpoint stability with interactive feel.
    #[default]
    Balanced,
    /// Wait longer for phrase boundaries and finalization stability.
    Careful,
}

impl SonioxLatencyMode {
    pub const fn storage_value(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Balanced => "balanced",
            Self::Careful => "careful",
        }
    }

    pub fn from_storage_value(value: &str) -> Option<Self> {
        match value {
            "fast" => Some(Self::Fast),
            "balanced" => Some(Self::Balanced),
            "careful" => Some(Self::Careful),
            _ => None,
        }
    }

    pub const fn max_endpoint_delay_ms(self) -> u32 {
        match self {
            Self::Fast => 500,
            Self::Balanced => 1_500,
            Self::Careful => 3_000,
        }
    }

    pub const fn finalize_settle_ms(self) -> u32 {
        match self {
            Self::Fast => 0,
            Self::Balanced => 250,
            Self::Careful => 750,
        }
    }
}

/// Soniox real-time STT session configuration.
#[derive(Clone, Debug)]
pub struct SonioxConfig {
    /// WebSocket URL. Defaults to the US real-time endpoint.
    pub websocket_url: String,
    /// Soniox STT model id.
    pub model: String,
    /// Raw audio format sent over the WebSocket.
    pub audio_format: String,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Number of audio channels.
    pub num_channels: u8,
    /// Enable Soniox speaker diarization.
    pub enable_speaker_diarization: bool,
    /// Enable Soniox semantic endpoint detection.
    pub enable_endpoint_detection: bool,
    /// Soniox endpoint detection delay. Higher values trade latency for more
    /// stable phrase boundaries.
    pub max_endpoint_delay_ms: Option<u32>,
    /// Optional Soniox context text. This biases recognition toward the
    /// session's domain/topic/vocabulary but does not enable future lookahead
    /// once tokens have been finalized.
    pub context_text: Option<String>,
}

impl Default for SonioxConfig {
    fn default() -> Self {
        Self {
            websocket_url: SONIOX_WS_URL.to_string(),
            model: "stt-rt-v4".to_string(),
            audio_format: "pcm_s16le".to_string(),
            sample_rate: 16_000,
            num_channels: 1,
            enable_speaker_diarization: true,
            enable_endpoint_detection: true,
            max_endpoint_delay_ms: None,
            context_text: None,
        }
    }
}

impl SonioxConfig {
    /// Build the default Soniox configuration with an explicit latency preset
    /// and optional context text.
    pub fn for_latency_mode_and_context(
        mode: SonioxLatencyMode,
        context_text: Option<String>,
    ) -> Self {
        Self {
            max_endpoint_delay_ms: Some(mode.max_endpoint_delay_ms()),
            context_text: context_text
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty()),
            ..Self::default()
        }
    }
}

fn config_payload(temp_api_key: &str, config: &SonioxConfig) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "api_key": temp_api_key,
        "model": config.model,
        "audio_format": config.audio_format,
        "sample_rate": config.sample_rate,
        "num_channels": config.num_channels,
        "enable_speaker_diarization": config.enable_speaker_diarization,
        "enable_endpoint_detection": config.enable_endpoint_detection,
    });

    if let Some(max_endpoint_delay_ms) = config.max_endpoint_delay_ms
        && let Some(object) = payload.as_object_mut()
    {
        object.insert(
            "max_endpoint_delay_ms".to_string(),
            serde_json::Value::from(max_endpoint_delay_ms),
        );
    }

    if let Some(context_text) = config.context_text.as_deref()
        && let Some(object) = payload.as_object_mut()
    {
        object.insert(
            "context".to_string(),
            serde_json::json!({ "text": context_text }),
        );
    }

    payload
}

/// Fetch a temporary Soniox WebSocket API key from the local proxy.
pub async fn fetch_temp_api_key() -> Result<String, String> {
    let mut last_err = String::new();

    for attempt in 0..3u32 {
        if attempt > 0 {
            let delay_ms = 500 * (1 << (attempt - 1));
            gloo_timers::future::TimeoutFuture::new(delay_ms).await;
        }

        match fetch_temp_api_key_once().await {
            Ok(token) => return Ok(token),
            Err(e) => {
                web_sys::console::warn_1(
                    &format!("Soniox token fetch attempt {} failed: {}", attempt + 1, e).into(),
                );
                last_err = e;
            }
        }
    }

    Err(last_err)
}

async fn fetch_temp_api_key_once() -> Result<String, String> {
    let window = web_sys::window().ok_or("no window")?;

    let opts = web_sys::RequestInit::new();
    opts.set_method("POST");

    let request = web_sys::Request::new_with_str_and_init(
        "http://127.0.0.1:3033/api/stt/soniox/token",
        &opts,
    )
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

    js_sys::Reflect::get(&json, &JsValue::from_str("api_key"))
        .map_err(|_| "no api_key field".to_string())?
        .as_string()
        .ok_or("api_key not a string".to_string())
}

/// Active Soniox real-time STT WebSocket session.
pub struct SonioxSession {
    ws: WebSocket,
    session_ready: Rc<Cell<bool>>,
}

impl SonioxSession {
    /// Open a Soniox WebSocket and send the initial configuration on connect.
    pub fn connect(
        temp_api_key: &str,
        config: SonioxConfig,
        on_event: impl FnMut(SttStreamEvent) + 'static,
        on_close: impl FnMut(u16, String) + 'static,
    ) -> Result<Self, JsValue> {
        web_sys::console::log_1(&"Connecting to Soniox STT WebSocket".into());

        let ws = WebSocket::new(&config.websocket_url)?;
        ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

        let session_ready = Rc::new(Cell::new(false));

        {
            let ws_for_open = ws.clone();
            let session_ready_open = session_ready.clone();
            let api_key = temp_api_key.to_string();
            let on_open = Closure::<dyn FnMut()>::new(move || {
                let payload = config_payload(&api_key, &config);
                match ws_for_open.send_with_str(&payload.to_string()) {
                    Ok(()) => {
                        session_ready_open.set(true);
                        web_sys::console::log_1(&"Soniox session configured".into());
                    }
                    Err(e) => {
                        web_sys::console::error_1(
                            &format!("Soniox config send failed: {e:?}").into(),
                        );
                    }
                }
            });
            ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
            on_open.forget();
        }

        {
            let mut on_event = on_event;
            let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                if let Ok(text) = event.data().dyn_into::<js_sys::JsString>() {
                    let text: String = text.into();
                    let preview: String = text.chars().take(300).collect();
                    web_sys::console::log_1(&format!("Soniox msg: {preview}").into());
                    for parsed in parse_soniox_events(&text) {
                        on_event(parsed);
                    }
                }
            });
            ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
            on_message.forget();
        }

        {
            let on_err = Closure::<dyn FnMut(web_sys::Event)>::new(move |_event| {
                web_sys::console::error_1(&"Soniox WebSocket error event fired".into());
            });
            ws.set_onerror(Some(on_err.as_ref().unchecked_ref()));
            on_err.forget();
        }

        {
            let mut on_close = on_close;
            let on_cls = Closure::<dyn FnMut(web_sys::CloseEvent)>::new(
                move |event: web_sys::CloseEvent| {
                    let code = event.code();
                    let reason = event.reason();
                    web_sys::console::log_1(
                        &format!("Soniox WebSocket closed: code={code}, reason={reason}").into(),
                    );
                    on_close(code, reason);
                },
            );
            ws.set_onclose(Some(on_cls.as_ref().unchecked_ref()));
            on_cls.forget();
        }

        Ok(Self { ws, session_ready })
    }

    /// Send raw PCM audio data as binary `pcm_s16le` bytes.
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

    /// Ask Soniox to finalize all audio sent so far while keeping the session open.
    pub fn finalize(&self) -> Result<(), JsValue> {
        if self.ws.ready_state() == WebSocket::OPEN {
            self.ws.send_with_str(r#"{"type":"finalize"}"#)?;
        }
        Ok(())
    }

    /// Keep an idle session alive when no audio frames are being sent.
    #[allow(dead_code)]
    pub fn keepalive(&self) -> Result<(), JsValue> {
        if self.ws.ready_state() == WebSocket::OPEN {
            self.ws.send_with_str(r#"{"type":"keepalive"}"#)?;
        }
        Ok(())
    }

    /// Gracefully end the stream. Soniox returns `finished: true` before close.
    pub fn finish(&self) -> Result<(), JsValue> {
        if self.ws.ready_state() == WebSocket::OPEN {
            self.ws.send_with_str("")?;
        }
        Ok(())
    }
}

/// Parse one Soniox JSON WebSocket message into provider-neutral events.
pub fn parse_soniox_events(text: &str) -> Vec<SttStreamEvent> {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) else {
        return vec![SttStreamEvent::Error {
            code: None,
            message: "invalid Soniox JSON response".to_string(),
        }];
    };

    if parsed.get("error_code").is_some() || parsed.get("error_message").is_some() {
        return vec![SttStreamEvent::Error {
            code: parsed
                .get("error_code")
                .and_then(|v| v.as_u64())
                .and_then(|v| u16::try_from(v).ok()),
            message: parsed
                .get("error_message")
                .and_then(|v| v.as_str())
                .unwrap_or("Soniox provider error")
                .to_string(),
        }];
    }

    let mut events = Vec::new();
    if let Some(tokens_value) = parsed.get("tokens")
        && let Some(tokens) = parse_tokens(tokens_value)
    {
        events.push(SttStreamEvent::Tokens {
            tokens,
            final_audio_proc_ms: parsed.get("final_audio_proc_ms").and_then(|v| v.as_f64()),
            total_audio_proc_ms: parsed.get("total_audio_proc_ms").and_then(|v| v.as_f64()),
        });
    }

    if parsed
        .get("finished")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        events.push(SttStreamEvent::Marker(SttMarker::Finished));
    }

    events
}

fn parse_tokens(value: &serde_json::Value) -> Option<Vec<SttToken>> {
    let arr = value.as_array()?;
    Some(
        arr.iter()
            .filter_map(|token| {
                let text = token.get("text").and_then(|v| v.as_str())?.to_string();
                let speaker_label = token.get("speaker").and_then(|speaker| {
                    speaker
                        .as_str()
                        .map(str::to_string)
                        .or_else(|| speaker.as_u64().map(|n| n.to_string()))
                });
                Some(SttToken {
                    text,
                    start_ms: token.get("start_ms").and_then(|v| v.as_f64()),
                    end_ms: token.get("end_ms").and_then(|v| v.as_f64()),
                    confidence: token
                        .get("confidence")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(1.0) as f32,
                    is_final: token
                        .get("is_final")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    speaker_label,
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_mode_storage_values_are_stable() {
        assert_eq!(SonioxLatencyMode::Fast.storage_value(), "fast");
        assert_eq!(SonioxLatencyMode::Balanced.storage_value(), "balanced");
        assert_eq!(SonioxLatencyMode::Careful.storage_value(), "careful");

        assert_eq!(
            SonioxLatencyMode::from_storage_value("fast"),
            Some(SonioxLatencyMode::Fast)
        );
        assert_eq!(
            SonioxLatencyMode::from_storage_value("balanced"),
            Some(SonioxLatencyMode::Balanced)
        );
        assert_eq!(
            SonioxLatencyMode::from_storage_value("careful"),
            Some(SonioxLatencyMode::Careful)
        );
        assert_eq!(SonioxLatencyMode::from_storage_value("assemblyai"), None);
    }

    #[test]
    fn latency_mode_maps_to_soniox_only_timing_values() {
        assert_eq!(SonioxLatencyMode::Fast.max_endpoint_delay_ms(), 500);
        assert_eq!(SonioxLatencyMode::Balanced.max_endpoint_delay_ms(), 1_500);
        assert_eq!(SonioxLatencyMode::Careful.max_endpoint_delay_ms(), 3_000);

        assert_eq!(SonioxLatencyMode::Fast.finalize_settle_ms(), 0);
        assert_eq!(SonioxLatencyMode::Balanced.finalize_settle_ms(), 250);
        assert_eq!(SonioxLatencyMode::Careful.finalize_settle_ms(), 750);
    }

    #[test]
    fn config_payload_omits_endpoint_delay_without_latency_mode() {
        let payload = config_payload("temp-key", &SonioxConfig::default());

        assert_eq!(payload["api_key"], "temp-key");
        assert_eq!(payload["model"], "stt-rt-v4");
        assert!(payload.get("max_endpoint_delay_ms").is_none());
    }

    #[test]
    fn config_payload_includes_soniox_latency_delay_when_set() {
        let config = SonioxConfig::for_latency_mode_and_context(SonioxLatencyMode::Careful, None);
        let payload = config_payload("temp-key", &config);

        assert_eq!(payload["max_endpoint_delay_ms"], 3_000);
    }

    #[test]
    fn config_payload_includes_soniox_context_text_when_set() {
        let config = SonioxConfig::for_latency_mode_and_context(
            SonioxLatencyMode::Balanced,
            Some(" Cardiology consult. Discuss mitral valve repair. ".to_string()),
        );
        let payload = config_payload("temp-key", &config);

        assert_eq!(
            payload["context"]["text"],
            "Cardiology consult. Discuss mitral valve repair."
        );
    }

    #[test]
    fn config_payload_omits_blank_soniox_context_text() {
        let config = SonioxConfig::for_latency_mode_and_context(
            SonioxLatencyMode::Balanced,
            Some("   \n\t  ".to_string()),
        );
        let payload = config_payload("temp-key", &config);

        assert!(payload.get("context").is_none());
    }

    #[test]
    fn parse_successful_token_response() {
        let events = parse_soniox_events(
            r#"{
                "tokens": [
                    {"text":"Hello","start_ms":600,"end_ms":760,"confidence":0.97,"is_final":true,"speaker":"1","language":"en"}
                ],
                "final_audio_proc_ms": 760,
                "total_audio_proc_ms": 880
            }"#,
        );

        assert_eq!(events.len(), 1);
        let SttStreamEvent::Tokens {
            tokens,
            final_audio_proc_ms,
            total_audio_proc_ms,
        } = &events[0]
        else {
            panic!("expected token event");
        };
        assert_eq!(*final_audio_proc_ms, Some(760.0));
        assert_eq!(*total_audio_proc_ms, Some(880.0));
        assert_eq!(tokens[0].text, "Hello");
        assert_eq!(tokens[0].speaker_label.as_deref(), Some("1"));
        assert!(tokens[0].is_final);
    }

    #[test]
    fn parse_finished_response_emits_finished_marker() {
        let events = parse_soniox_events(
            r#"{"tokens":[],"final_audio_proc_ms":1560,"total_audio_proc_ms":1680,"finished":true}"#,
        );

        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[1],
            SttStreamEvent::Marker(SttMarker::Finished)
        ));
    }

    #[test]
    fn parse_error_response_emits_error_event() {
        let events = parse_soniox_events(
            r#"{"tokens":[],"error_code":503,"error_message":"Cannot continue request"}"#,
        );

        assert_eq!(
            events,
            vec![SttStreamEvent::Error {
                code: Some(503),
                message: "Cannot continue request".to_string(),
            }]
        );
    }

    #[test]
    fn parse_invalid_json_emits_error_event() {
        let events = parse_soniox_events("not json");
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], SttStreamEvent::Error { .. }));
    }

    #[test]
    fn parse_numeric_speaker_label_as_string() {
        let events = parse_soniox_events(
            r#"{"tokens":[{"text":"Hi","confidence":0.9,"is_final":true,"speaker":2}]}"#,
        );
        let SttStreamEvent::Tokens { tokens, .. } = &events[0] else {
            panic!("expected token event");
        };
        assert_eq!(tokens[0].speaker_label.as_deref(), Some("2"));
    }
}
