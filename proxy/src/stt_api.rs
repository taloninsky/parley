//! HTTP surface for STT.
//!
//! Spec: `docs/xai-speech-integration-spec.md` §8.1–§8.2.
//!
//! Carries two routes: the batch REST handler
//! (`POST /api/stt/transcribe`) and the streaming WebSocket bridge
//! (`GET /api/stt/stream`). The orchestrator-side provider selection
//! (Step 6) replaces this module's inline per-request factory with a
//! router once it exists.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use futures::StreamExt;
use futures::stream::BoxStream;
use parley_core::stt::{SttAudioFormat, SttRequest, SttStreamConfig, Transcript, TranscriptEvent};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::providers::{ProviderId, UnknownProvider};
use crate::secrets::SecretsManager;
use crate::stt::{SttError, SttProvider, SttResult, SttStreamHandle, XaiStt};

/// Shared state for the STT API router.
#[derive(Clone)]
pub struct SttApiState {
    /// HTTP client reused across outbound xAI calls.
    pub client: reqwest::Client,
    /// Credential resolver — same instance the rest of the proxy uses.
    pub secrets: Arc<SecretsManager>,
}

impl SttApiState {
    /// Construct.
    pub fn new(client: reqwest::Client, secrets: Arc<SecretsManager>) -> Self {
        Self { client, secrets }
    }
}

/// Build the STT sub-router. Mounted at the root — routes carry the
/// full `/api/stt/...` prefix.
pub fn router(state: SttApiState) -> Router {
    Router::new()
        .route("/api/stt/transcribe", post(transcribe))
        .route("/api/stt/stream", get(stream_ws))
        .with_state(state)
}

// ── request body ──────────────────────────────────────────────────────

/// Body of `POST /api/stt/transcribe` (spec §8.1).
#[derive(Debug, Deserialize)]
struct TranscribeRequest {
    /// Provider id (e.g. `"xai"`). Must be a known STT provider.
    provider: String,
    /// Credential name the provider should resolve; defaults to
    /// `"default"` if omitted.
    #[serde(default = "default_credential")]
    credential: String,
    /// Audio-file provider config (language, diarization, …).
    #[serde(default)]
    config: TranscribeConfig,
    /// Audio source.
    audio: AudioSource,
}

fn default_credential() -> String {
    crate::secrets::DEFAULT_CREDENTIAL.to_string()
}

/// Optional per-request STT config. All fields are advisory — unknown
/// fields are quietly ignored so the schema can grow without breaking
/// older clients.
#[derive(Debug, Default, Deserialize)]
struct TranscribeConfig {
    #[serde(default)]
    language: Option<String>,
    #[serde(default = "default_diarize")]
    diarize: bool,
    // `model` and `format` (inverse text normalization) are accepted for
    // forward compatibility (§8.1) but not yet passed through.
    #[serde(default)]
    #[allow(dead_code)]
    model: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    format: Option<bool>,
}

fn default_diarize() -> bool {
    true
}

/// Where the audio bytes come from.
///
/// The spec allows `{ source: "url", url: "…" }` as well; that path
/// requires the proxy to fetch the URL server-side and is left as a
/// follow-up. `inline_base64` covers file-pick uploads in the UI.
#[derive(Debug, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
enum AudioSource {
    /// Inline base64-encoded audio plus a format hint.
    InlineBase64 {
        /// Base64 bytes (standard alphabet, padding optional).
        data: String,
        /// Container/codec hint so the provider knows how to decode.
        audio_format: InlineAudioFormat,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum InlineAudioFormat {
    Wav,
    Mp3,
    Opus,
    Flac,
    /// PCM16 LE — `sample_rate_hz` required.
    Pcm16Le {
        sample_rate_hz: u32,
    },
}

impl From<InlineAudioFormat> for SttAudioFormat {
    fn from(f: InlineAudioFormat) -> SttAudioFormat {
        match f {
            InlineAudioFormat::Wav => SttAudioFormat::Wav,
            InlineAudioFormat::Mp3 => SttAudioFormat::Mp3,
            InlineAudioFormat::Opus => SttAudioFormat::Opus,
            InlineAudioFormat::Flac => SttAudioFormat::Flac,
            InlineAudioFormat::Pcm16Le { sample_rate_hz } => {
                SttAudioFormat::Pcm16Le { sample_rate_hz }
            }
        }
    }
}

// ── handler ───────────────────────────────────────────────────────────

async fn transcribe(
    State(state): State<SttApiState>,
    Json(body): Json<TranscribeRequest>,
) -> (StatusCode, Json<Value>) {
    let provider_id: ProviderId = match body.provider.parse() {
        Ok(p) => p,
        Err(UnknownProvider(raw)) => {
            return bad_request(
                "unknown_provider",
                &format!("{raw} is not a known provider"),
            );
        }
    };

    let api_key = match state.secrets.resolve(provider_id, &body.credential) {
        Some(k) => k,
        None => return provider_not_configured(provider_id, &body.credential),
    };

    let (audio_bytes, format) = match body.audio {
        AudioSource::InlineBase64 { data, audio_format } => {
            match base64::engine::general_purpose::STANDARD.decode(data.as_bytes()) {
                Ok(bytes) => (bytes, SttAudioFormat::from(audio_format)),
                Err(e) => {
                    return bad_request("invalid_base64", &format!("audio.data: {e}"));
                }
            }
        }
    };

    let provider: Box<dyn SttProvider> = match provider_id {
        ProviderId::Xai => Box::new(XaiStt::new(api_key, state.client.clone())),
        other => {
            return bad_request(
                "unsupported_provider_for_stt",
                &format!("{} cannot transcribe files", other.as_str()),
            );
        }
    };

    let req = SttRequest {
        audio: audio_bytes,
        format,
        language: body.config.language,
        diarize: body.config.diarize,
    };

    match provider.transcribe(req).await {
        Ok(t) => (StatusCode::OK, Json(transcript_to_json(&t))),
        Err(e) => map_stt_error(provider_id, e),
    }
}

// ── response helpers ──────────────────────────────────────────────────

fn transcript_to_json(t: &Transcript) -> Value {
    serde_json::to_value(t).unwrap_or_else(|_| json!({}))
}

fn provider_not_configured(provider: ProviderId, credential: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::PRECONDITION_FAILED,
        Json(json!({
            "error": "provider_not_configured",
            "provider": provider.as_str(),
            "credential": credential,
        })),
    )
}

fn bad_request(error: &str, detail: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": error, "detail": detail })),
    )
}

/// Project [`SttError`] onto the HTTP failure mapping from §8.6.
fn map_stt_error(provider: ProviderId, err: SttError) -> (StatusCode, Json<Value>) {
    match err {
        SttError::Auth(body) => (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": "provider_auth_failed",
                "provider": provider.as_str(),
                "detail": body,
            })),
        ),
        SttError::Http { status, body } => {
            // 429 passes through so callers can honor Retry-After
            // themselves; 5xx collapses to 502 per spec §8.6 (no raw
            // upstream body leak).
            if status == 429 {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({
                        "error": "upstream_rate_limited",
                        "provider": provider.as_str(),
                        "detail": body,
                    })),
                )
            } else if (500..=599).contains(&status) {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({
                        "error": "upstream_error",
                        "provider": provider.as_str(),
                    })),
                )
            } else {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({
                        "error": "upstream_http_error",
                        "provider": provider.as_str(),
                        "status": status,
                    })),
                )
            }
        }
        SttError::Transport(detail) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": "upstream_transport_error",
                "provider": provider.as_str(),
                "detail": detail,
            })),
        ),
        SttError::BadResponse(detail) | SttError::Protocol(detail) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": "upstream_protocol_error",
                "provider": provider.as_str(),
                "detail": detail,
            })),
        ),
        SttError::Unsupported(detail) => (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({
                "error": "unsupported",
                "provider": provider.as_str(),
                "detail": detail,
            })),
        ),
        SttError::Other(detail) => bad_request("invalid_request", &detail),
    }
}

// ── WebSocket bridge (§8.2) ───────────────────────────────────────────

/// Query parameters for `GET /api/stt/stream`. See spec §8.2.
#[derive(Debug, Deserialize)]
struct StreamQuery {
    provider: String,
    #[serde(default = "default_credential")]
    credential: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default = "default_diarize")]
    diarize: bool,
    /// PCM16 LE sample rate in Hz. Streaming is PCM-only in v1 per
    /// `parley_core::stt::SttStreamConfig`'s `format` doc.
    #[serde(default = "default_sample_rate")]
    sample_rate: u32,
}

fn default_sample_rate() -> u32 {
    16_000
}

async fn stream_ws(
    State(state): State<SttApiState>,
    Query(q): Query<StreamQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let provider_id: ProviderId = match q.provider.parse() {
        Ok(p) => p,
        Err(UnknownProvider(raw)) => {
            return bad_request(
                "unknown_provider",
                &format!("{raw} is not a known provider"),
            )
            .into_response();
        }
    };

    let api_key = match state.secrets.resolve(provider_id, &q.credential) {
        Some(k) => k,
        None => return provider_not_configured(provider_id, &q.credential).into_response(),
    };

    let provider: Box<dyn SttProvider> = match provider_id {
        ProviderId::Xai => Box::new(XaiStt::new(api_key, state.client.clone())),
        other => {
            return bad_request(
                "unsupported_provider_for_stt_stream",
                &format!("{} cannot stream transcription", other.as_str()),
            )
            .into_response();
        }
    };

    let config = SttStreamConfig {
        format: SttAudioFormat::Pcm16Le {
            sample_rate_hz: q.sample_rate,
        },
        language: q.language,
        diarize: q.diarize,
    };

    let handle = match provider.stream(config).await {
        Ok(h) => h,
        Err(e) => {
            let (status, body) = map_stt_error(provider_id, e);
            return (status, body).into_response();
        }
    };

    ws.on_upgrade(move |socket| run_bridge(socket, handle))
}

/// Spawn forward / backward pumps between the browser WS and the
/// provider's `SttStreamHandle`. Returns when both pumps complete.
async fn run_bridge(socket: WebSocket, handle: SttStreamHandle) {
    let (ws_sink, ws_source) = socket.split();
    let SttStreamHandle { audio_tx, events } = handle;
    let client_to_provider = pump_client_to_provider(ws_source, audio_tx);
    let provider_to_client = pump_provider_to_client(events, ws_sink);
    // Run both until terminal on either side.
    tokio::join!(client_to_provider, provider_to_client);
}

/// Pump browser → provider: binary frames forward, control text
/// `{"type":"audio.done"}` closes the provider's audio sink.
async fn pump_client_to_provider<S>(mut ws_source: S, audio_tx: mpsc::Sender<Vec<u8>>)
where
    S: futures::Stream<Item = Result<Message, axum::Error>> + Unpin,
{
    while let Some(next) = ws_source.next().await {
        let Ok(msg) = next else { break };
        match msg {
            Message::Binary(bytes) => {
                if audio_tx.send(bytes.to_vec()).await.is_err() {
                    break;
                }
            }
            Message::Text(text) => {
                if is_audio_done(&text) {
                    break;
                }
                // Other text frames are ignored — the browser isn't
                // expected to send transcript-control messages today.
            }
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }
    // Dropping audio_tx here signals EOF to the provider.
    drop(audio_tx);
}

fn is_audio_done(text: &str) -> bool {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_string))
        .as_deref()
        == Some("audio.done")
}

/// Pump provider → browser: `TranscriptEvent` JSON frames forward,
/// errors serialize as `{"type":"error","message":"…"}` per §8.6, and
/// the stream closes after `Done` or any fatal error.
async fn pump_provider_to_client<K>(
    mut events: BoxStream<'static, SttResult<TranscriptEvent>>,
    mut ws_sink: K,
) where
    K: futures::Sink<Message, Error = axum::Error> + Unpin,
{
    use futures::SinkExt;

    while let Some(item) = events.next().await {
        match item {
            Ok(ev) => {
                let payload = match serde_json::to_string(&ev) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ws_sink
                            .send(error_frame(&format!("encode failed: {e}")))
                            .await;
                        break;
                    }
                };
                let is_terminal = matches!(ev, TranscriptEvent::Done { .. });
                if ws_sink.send(Message::Text(payload.into())).await.is_err() {
                    break;
                }
                if is_terminal {
                    break;
                }
            }
            Err(e) => {
                let _ = ws_sink.send(error_frame(&e.to_string())).await;
                break;
            }
        }
    }
    let _ = ws_sink.close().await;
}

fn error_frame(message: &str) -> Message {
    Message::Text(
        json!({ "type": "error", "message": message })
            .to_string()
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn secrets_with(xai_key: Option<&str>) -> Arc<SecretsManager> {
        let store = crate::secrets::InMemoryKeyStore::new();
        let index_path = std::env::temp_dir().join(format!(
            "parley-stt-api-test-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let manager = Arc::new(SecretsManager::new(
            Box::new(store),
            Box::new(crate::secrets::StaticEnv::new()),
            index_path,
        ));
        if let Some(k) = xai_key {
            manager
                .set(ProviderId::Xai, crate::secrets::DEFAULT_CREDENTIAL, k)
                .expect("seed xai credential");
        }
        manager
    }

    fn app_with(secrets: Arc<SecretsManager>) -> Router {
        router(SttApiState::new(reqwest::Client::new(), secrets))
    }

    async fn body_to_value(resp: axum::response::Response) -> (StatusCode, Value) {
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v)
    }

    #[tokio::test]
    async fn transcribe_missing_credential_returns_412() {
        let app = app_with(secrets_with(None));
        let req = Request::builder()
            .method("POST")
            .uri("/api/stt/transcribe")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"provider":"xai","credential":"default","audio":{"source":"inline_base64","data":"AAAA","audio_format":"wav"}}"#,
            ))
            .unwrap();
        let (status, body) = body_to_value(app.oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::PRECONDITION_FAILED);
        assert_eq!(body["error"], "provider_not_configured");
        assert_eq!(body["provider"], "xai");
    }

    #[tokio::test]
    async fn transcribe_unknown_provider_returns_400() {
        let app = app_with(secrets_with(None));
        let req = Request::builder()
            .method("POST")
            .uri("/api/stt/transcribe")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"provider":"made-up","audio":{"source":"inline_base64","data":"AA","audio_format":"wav"}}"#,
            ))
            .unwrap();
        let (status, body) = body_to_value(app.oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "unknown_provider");
    }

    #[tokio::test]
    async fn transcribe_invalid_base64_returns_400() {
        let app = app_with(secrets_with(Some("xai-k")));
        let req = Request::builder()
            .method("POST")
            .uri("/api/stt/transcribe")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"provider":"xai","audio":{"source":"inline_base64","data":"!!!","audio_format":"wav"}}"#,
            ))
            .unwrap();
        let (status, body) = body_to_value(app.oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_base64");
    }

    #[test]
    fn map_stt_error_auth_goes_to_401() {
        let (status, body) = map_stt_error(ProviderId::Xai, SttError::Auth("bad".into()));
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body.0["error"], "provider_auth_failed");
    }

    #[test]
    fn map_stt_error_429_preserves_and_returns_429() {
        let (status, body) = map_stt_error(
            ProviderId::Xai,
            SttError::Http {
                status: 429,
                body: "slow down".into(),
            },
        );
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body.0["error"], "upstream_rate_limited");
    }

    #[test]
    fn map_stt_error_5xx_collapses_to_502_without_body_leak() {
        let (status, body) = map_stt_error(
            ProviderId::Xai,
            SttError::Http {
                status: 503,
                body: "internal detail not for clients".into(),
            },
        );
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body.0["error"], "upstream_error");
        // Spec §8.6: don't leak raw upstream body to callers.
        assert!(body.0.get("detail").is_none());
        assert!(body.0.get("body").is_none());
    }

    #[test]
    fn map_stt_error_unsupported_returns_501() {
        let (status, _body) = map_stt_error(ProviderId::Xai, SttError::Unsupported("no ws".into()));
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    }

    #[test]
    fn is_audio_done_recognizes_control_frame() {
        assert!(is_audio_done(r#"{"type":"audio.done"}"#));
        assert!(is_audio_done(r#"{"type":"audio.done","extra":1}"#));
        assert!(!is_audio_done(r#"{"type":"audio.data"}"#));
        assert!(!is_audio_done(r#"{"foo":"bar"}"#));
        assert!(!is_audio_done("not json"));
    }

    // ── WS bridge unit tests ─────────────────────────────────────────
    //
    // The bridge pumps are generic over sink/stream types, so we wire
    // them up with `futures::channel::mpsc` in place of a real axum
    // WebSocket. This verifies message routing without standing up a
    // live xAI WS server. A full end-to-end WS integration test using
    // a mock upstream is the responsibility of the orchestrator step
    // (Step 6) where the router owns provider selection.

    use futures::SinkExt;
    use futures::channel::mpsc as fmpsc;
    #[tokio::test]
    async fn client_pump_forwards_binary_and_stops_on_audio_done() {
        let (mut ws_in, ws_out) = fmpsc::unbounded::<Result<Message, axum::Error>>();
        let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(8);

        ws_in
            .send(Ok(Message::Binary(vec![1u8, 2, 3].into())))
            .await
            .unwrap();
        ws_in
            .send(Ok(Message::Binary(vec![4u8, 5].into())))
            .await
            .unwrap();
        ws_in
            .send(Ok(Message::Text(r#"{"type":"audio.done"}"#.into())))
            .await
            .unwrap();
        // Keep sender alive so the pump only exits on audio.done.
        let pump = tokio::spawn(pump_client_to_provider(ws_out, audio_tx));

        let first = audio_rx.recv().await.unwrap();
        assert_eq!(first, vec![1, 2, 3]);
        let second = audio_rx.recv().await.unwrap();
        assert_eq!(second, vec![4, 5]);
        // After audio.done the sink is dropped → recv returns None.
        assert!(audio_rx.recv().await.is_none());
        pump.await.unwrap();
    }

    #[tokio::test]
    async fn provider_pump_forwards_events_and_closes_on_done() {
        let events = futures::stream::iter(vec![
            Ok(TranscriptEvent::Partial { text: "hel".into() }),
            Ok(TranscriptEvent::Final {
                text: "hello".into(),
                speaker: None,
                start_seconds: None,
                end_seconds: None,
            }),
            Ok(TranscriptEvent::Done {
                duration_seconds: 1.5,
            }),
        ])
        .boxed();

        let (ws_sink, mut ws_sink_out) = fmpsc::unbounded::<Message>();
        let sink_adapter =
            ws_sink.sink_map_err(|_| axum::Error::new(std::io::Error::other("unreachable")));

        pump_provider_to_client(events, sink_adapter).await;

        let mut frames = Vec::new();
        while let Some(m) = ws_sink_out.next().await {
            frames.push(m);
        }
        assert_eq!(frames.len(), 3, "three events, then sink is closed");

        let text0 = match &frames[0] {
            Message::Text(t) => t.as_str(),
            _ => panic!("expected text"),
        };
        assert!(text0.contains("\"kind\":\"partial\""));
        let text1 = match &frames[1] {
            Message::Text(t) => t.as_str(),
            _ => panic!("expected text"),
        };
        assert!(text1.contains("\"kind\":\"final\""));
        let text2 = match &frames[2] {
            Message::Text(t) => t.as_str(),
            _ => panic!("expected text"),
        };
        assert!(text2.contains("\"kind\":\"done\""));
    }

    #[tokio::test]
    async fn provider_pump_emits_error_frame_on_stream_error() {
        let events = futures::stream::iter(vec![Err(SttError::Protocol("boom".into()))]).boxed();

        let (ws_sink, mut ws_sink_out) = fmpsc::unbounded::<Message>();
        let sink_adapter =
            ws_sink.sink_map_err(|_| axum::Error::new(std::io::Error::other("unreachable")));

        pump_provider_to_client(events, sink_adapter).await;

        let frame = ws_sink_out.next().await.expect("error frame");
        let text = match &frame {
            Message::Text(t) => t.as_str(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("boom"));
        assert!(
            ws_sink_out.next().await.is_none(),
            "sink closed after error"
        );
    }

    // HTTP-layer unit tests for the WS upgrade extractor are skipped:
    // `axum::extract::WebSocketUpgrade` validates upgrade headers
    // before our handler runs, and a `tower::oneshot` request can't
    // stand up the connection the extractor expects. The bridge pump
    // logic is covered by the `client_pump_*` / `provider_pump_*` tests
    // above, and the query-param / provider-factory / error-mapping
    // paths are reused verbatim from `transcribe`, which IS covered by
    // HTTP-layer tests. A full end-to-end WS roundtrip against a mock
    // xAI server is a Step 6 concern once the orchestrator owns
    // provider selection.
}
