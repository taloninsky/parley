//! HTTP surface for STT.
//!
//! Spec: `docs/xai-speech-integration-spec.md` §8.1–§8.2.
//!
//! This module lands the REST path first. The WebSocket bridge
//! (`GET /api/stt/stream`) is a sibling commit in Step 5 WS; the
//! orchestrator-side provider selection (Step 6) replaces this
//! module's inline per-request factory with a router once it exists.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine;
use parley_core::stt::{SttAudioFormat, SttRequest, Transcript};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::providers::{ProviderId, UnknownProvider};
use crate::secrets::SecretsManager;
use crate::stt::{SttError, SttProvider, XaiStt};

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
            return bad_request("unknown_provider", &format!("{raw} is not a known provider"));
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

fn provider_not_configured(
    provider: ProviderId,
    credential: &str,
) -> (StatusCode, Json<Value>) {
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
        let (status, _body) =
            map_stt_error(ProviderId::Xai, SttError::Unsupported("no ws".into()));
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    }
}
