//! HTTP surface for TTS.
//!
//! Spec: `docs/xai-speech-integration-spec.md` §8.3.
//!
//! Unary (non-streaming) `POST /api/tts/synthesize` and the voice
//! catalog endpoint `GET /api/tts/voices` land here. The streaming
//! bridge (`GET /api/tts/stream`) is deferred to Step 4 WS per spec
//! §12.1.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::providers::{ProviderId, UnknownProvider};
use crate::secrets::SecretsManager;
use crate::tts::xai::XaiTts;
use crate::tts::{
    AudioFormat, ElevenLabsTts, SynthesisContext, TtsChunk, TtsError, TtsProvider, TtsRequest,
};

/// Shared state for the TTS API router.
#[derive(Clone)]
pub struct TtsApiState {
    /// HTTP client reused across outbound provider calls.
    pub client: reqwest::Client,
    /// Credential resolver.
    pub secrets: Arc<SecretsManager>,
}

impl TtsApiState {
    /// Construct.
    pub fn new(client: reqwest::Client, secrets: Arc<SecretsManager>) -> Self {
        Self { client, secrets }
    }
}

/// Build the TTS sub-router.
pub fn router(state: TtsApiState) -> Router {
    Router::new()
        .route("/api/tts/synthesize", post(synthesize))
        .route("/api/tts/voices", get(voices))
        .with_state(state)
}

// ── request body ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SynthesizeRequest {
    /// Provider id (e.g. `"xai"`, `"elevenlabs"`).
    provider: String,
    /// Credential name; defaults to `"default"`.
    #[serde(default = "default_credential")]
    credential: String,
    /// Provider-specific voice id.
    voice_id: String,
    /// Text to synthesize. Must be non-empty.
    text: String,
}

fn default_credential() -> String {
    crate::secrets::DEFAULT_CREDENTIAL.to_string()
}

// ── handler ───────────────────────────────────────────────────────────

async fn synthesize(
    State(state): State<TtsApiState>,
    Json(body): Json<SynthesizeRequest>,
) -> (StatusCode, Json<Value>) {
    if body.text.is_empty() {
        return bad_request("empty_text", "text must be non-empty");
    }

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

    let provider: Box<dyn TtsProvider> = match provider_id {
        ProviderId::Xai => Box::new(XaiTts::new(api_key, state.client.clone())),
        ProviderId::ElevenLabs => Box::new(ElevenLabsTts::new(api_key, state.client.clone())),
        other => {
            return bad_request(
                "unsupported_provider_for_tts",
                &format!("{} cannot synthesize speech", other.as_str()),
            );
        }
    };

    let output_format = provider.output_format();

    let req = TtsRequest {
        voice_id: body.voice_id,
        text: body.text,
    };

    let mut stream = match provider.synthesize(req, SynthesisContext::default()).await {
        Ok(s) => s,
        Err(e) => return map_tts_error(provider_id, e),
    };

    let mut audio = Vec::new();
    let mut characters: u32 = 0;
    while let Some(item) = stream.next().await {
        match item {
            Ok(TtsChunk::Audio(b)) => audio.extend(b),
            Ok(TtsChunk::Done { characters: c }) => characters = c,
            Err(e) => return map_tts_error(provider_id, e),
        }
    }

    let cost = provider.cost(characters);
    let b64 = base64::engine::general_purpose::STANDARD.encode(&audio);

    (
        StatusCode::OK,
        Json(json!({
            "provider": provider_id.as_str(),
            "audio_base64": b64,
            "audio_format": audio_format_str(output_format),
            "characters": characters,
            "cost_usd": cost.usd,
        })),
    )
}

// ── voices catalog ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct VoicesQuery {
    provider: String,
    #[serde(default = "default_credential")]
    credential: String,
}

async fn voices(
    State(state): State<TtsApiState>,
    Query(q): Query<VoicesQuery>,
) -> (StatusCode, Json<Value>) {
    let provider_id: ProviderId = match q.provider.parse() {
        Ok(p) => p,
        Err(UnknownProvider(raw)) => {
            return bad_request(
                "unknown_provider",
                &format!("{raw} is not a known provider"),
            );
        }
    };

    let api_key = match state.secrets.resolve(provider_id, &q.credential) {
        Some(k) => k,
        None => return provider_not_configured(provider_id, &q.credential),
    };

    let provider: Box<dyn TtsProvider> = match provider_id {
        ProviderId::Xai => Box::new(XaiTts::new(api_key, state.client.clone())),
        ProviderId::ElevenLabs => Box::new(ElevenLabsTts::new(api_key, state.client.clone())),
        other => {
            return bad_request(
                "unsupported_provider_for_tts",
                &format!("{} cannot synthesize speech", other.as_str()),
            );
        }
    };

    match provider.voices().await {
        Ok(list) => (
            StatusCode::OK,
            Json(json!({
                "provider": provider_id.as_str(),
                "voices": list,
            })),
        ),
        Err(e) => map_tts_error(provider_id, e),
    }
}

fn audio_format_str(f: AudioFormat) -> &'static str {
    match f {
        AudioFormat::Mp3_44100_128 => "mp3_44100_128",
    }
}

// ── response helpers ──────────────────────────────────────────────────

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

/// Project [`TtsError`] onto the HTTP failure mapping from §8.6.
fn map_tts_error(provider: ProviderId, err: TtsError) -> (StatusCode, Json<Value>) {
    match err {
        TtsError::Http { status, body } => {
            if status == 401 || status == 403 {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({
                        "error": "provider_auth_failed",
                        "provider": provider.as_str(),
                        "detail": body,
                    })),
                )
            } else if status == 429 {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({
                        "error": "upstream_rate_limited",
                        "provider": provider.as_str(),
                        "detail": body,
                    })),
                )
            } else if (500..=599).contains(&status) {
                // Spec §8.6: don't leak raw upstream body on 5xx.
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
        TtsError::Transport(detail) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": "upstream_transport_error",
                "provider": provider.as_str(),
                "detail": detail,
            })),
        ),
        TtsError::Protocol(detail) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": "upstream_protocol_error",
                "provider": provider.as_str(),
                "detail": detail,
            })),
        ),
        TtsError::Other(detail) => bad_request("invalid_request", &detail),
        TtsError::Unsupported(detail) => (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({
                "error": "unsupported",
                "provider": provider.as_str(),
                "detail": detail,
            })),
        ),
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
            "parley-tts-api-test-{}-{}.json",
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
        router(TtsApiState::new(reqwest::Client::new(), secrets))
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
    async fn synthesize_empty_text_returns_400() {
        let app = app_with(secrets_with(Some("xai-k")));
        let req = Request::builder()
            .method("POST")
            .uri("/api/tts/synthesize")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"provider":"xai","voice_id":"eve","text":""}"#,
            ))
            .unwrap();
        let (status, body) = body_to_value(app.oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "empty_text");
    }

    #[tokio::test]
    async fn synthesize_unknown_provider_returns_400() {
        let app = app_with(secrets_with(None));
        let req = Request::builder()
            .method("POST")
            .uri("/api/tts/synthesize")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"provider":"made-up","voice_id":"eve","text":"hi"}"#,
            ))
            .unwrap();
        let (status, body) = body_to_value(app.oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "unknown_provider");
    }

    #[tokio::test]
    async fn synthesize_missing_credential_returns_412() {
        let app = app_with(secrets_with(None));
        let req = Request::builder()
            .method("POST")
            .uri("/api/tts/synthesize")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"provider":"xai","voice_id":"eve","text":"hi"}"#,
            ))
            .unwrap();
        let (status, body) = body_to_value(app.oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::PRECONDITION_FAILED);
        assert_eq!(body["error"], "provider_not_configured");
        assert_eq!(body["provider"], "xai");
    }

    #[tokio::test]
    async fn synthesize_unsupported_provider_returns_400() {
        let store = crate::secrets::InMemoryKeyStore::new();
        let index_path = std::env::temp_dir().join(format!(
            "parley-tts-api-test-llm-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let secrets = Arc::new(SecretsManager::new(
            Box::new(store),
            Box::new(crate::secrets::StaticEnv::new()),
            index_path,
        ));
        secrets
            .set(
                ProviderId::Anthropic,
                crate::secrets::DEFAULT_CREDENTIAL,
                "k",
            )
            .expect("seed anthropic credential");

        let app = app_with(secrets);
        let req = Request::builder()
            .method("POST")
            .uri("/api/tts/synthesize")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"provider":"anthropic","voice_id":"x","text":"hi"}"#,
            ))
            .unwrap();
        let (status, body) = body_to_value(app.oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "unsupported_provider_for_tts");
    }

    #[test]
    fn map_tts_error_401_goes_to_401() {
        let (status, body) = map_tts_error(
            ProviderId::Xai,
            TtsError::Http {
                status: 401,
                body: "bad key".into(),
            },
        );
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body.0["error"], "provider_auth_failed");
    }

    #[test]
    fn map_tts_error_429_preserves() {
        let (status, body) = map_tts_error(
            ProviderId::Xai,
            TtsError::Http {
                status: 429,
                body: "slow".into(),
            },
        );
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body.0["error"], "upstream_rate_limited");
    }

    #[test]
    fn map_tts_error_5xx_collapses_without_body_leak() {
        let (status, body) = map_tts_error(
            ProviderId::Xai,
            TtsError::Http {
                status: 503,
                body: "internal".into(),
            },
        );
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body.0["error"], "upstream_error");
        assert!(body.0.get("detail").is_none());
        assert!(body.0.get("body").is_none());
    }

    #[test]
    fn audio_format_str_stable() {
        assert_eq!(
            audio_format_str(AudioFormat::Mp3_44100_128),
            "mp3_44100_128"
        );
    }

    #[test]
    fn map_tts_error_unsupported_goes_to_501() {
        let (status, body) = map_tts_error(
            ProviderId::Xai,
            TtsError::Unsupported("voices catalog".into()),
        );
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(body.0["error"], "unsupported");
        assert_eq!(body.0["provider"], "xai");
    }

    #[tokio::test]
    async fn voices_unknown_provider_returns_400() {
        let app = app_with(secrets_with(None));
        let req = Request::builder()
            .method("GET")
            .uri("/api/tts/voices?provider=made-up")
            .body(Body::empty())
            .unwrap();
        let (status, body) = body_to_value(app.oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "unknown_provider");
    }

    #[tokio::test]
    async fn voices_missing_credential_returns_412() {
        let app = app_with(secrets_with(None));
        let req = Request::builder()
            .method("GET")
            .uri("/api/tts/voices?provider=xai")
            .body(Body::empty())
            .unwrap();
        let (status, body) = body_to_value(app.oneshot(req).await.unwrap()).await;
        assert_eq!(status, StatusCode::PRECONDITION_FAILED);
        assert_eq!(body["error"], "provider_not_configured");
        assert_eq!(body["provider"], "xai");
    }
}
