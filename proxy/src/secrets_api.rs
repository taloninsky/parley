//! HTTP surface for credential management.
//!
//! Spec reference: `docs/secrets-storage-spec.md` §5.
//!
//! Endpoints (all bound to localhost, no per-endpoint auth in this pass —
//! see spec §5.6 / §10.1):
//!
//! - `GET    /api/secrets/status`
//! - `PUT    /api/secrets/{provider}/{credential}`
//! - `DELETE /api/secrets/{provider}/{credential}`
//! - `POST   /api/secrets/{provider}/{credential}/rename`
//!
//! No endpoint ever returns a stored key value. `PUT`/`DELETE`/`rename`
//! return the updated single-credential status; `status` returns the
//! categorized view of every known provider.

use crate::providers::ProviderId;
use crate::secrets::{KeyStoreError, SecretsError, SecretsManager};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// Shared state for the secrets routes.
#[derive(Clone)]
pub struct SecretsApiState {
    /// The single in-process manager. `Arc` because the same handle is
    /// used by every route handler concurrently.
    pub manager: Arc<SecretsManager>,
}

impl SecretsApiState {
    /// Construct.
    pub fn new(manager: Arc<SecretsManager>) -> Self {
        Self { manager }
    }
}

/// Build the secrets sub-router. Mounted under `/api/secrets`.
pub fn router(state: SecretsApiState) -> Router {
    Router::new()
        .route("/api/secrets/status", get(status))
        .route("/api/secrets/{provider}/{credential}", put(set_credential))
        .route(
            "/api/secrets/{provider}/{credential}",
            delete(delete_credential),
        )
        .route(
            "/api/secrets/{provider}/{credential}/rename",
            post(rename_credential),
        )
        .with_state(state)
}

// ── handlers ───────────────────────────────────────────────────────────

async fn status(State(state): State<SecretsApiState>) -> impl IntoResponse {
    let report = state.manager.status();
    (StatusCode::OK, Json(report))
}

#[derive(Deserialize)]
struct SetBody {
    key: String,
}

async fn set_credential(
    State(state): State<SecretsApiState>,
    Path((provider, credential)): Path<(String, String)>,
    Json(body): Json<SetBody>,
) -> (StatusCode, Json<Value>) {
    let provider = match parse_provider(&provider) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    match state.manager.set(provider, &credential, &body.key) {
        Ok(status) => (
            StatusCode::OK,
            Json(serde_json::to_value(status).expect("CredentialStatus serializes")),
        ),
        Err(e) => secrets_error_response(e),
    }
}

async fn delete_credential(
    State(state): State<SecretsApiState>,
    Path((provider, credential)): Path<(String, String)>,
) -> (StatusCode, Json<Value>) {
    let provider = match parse_provider(&provider) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    match state.manager.delete(provider, &credential) {
        Ok(status) => (
            StatusCode::OK,
            Json(serde_json::to_value(status).expect("CredentialStatus serializes")),
        ),
        Err(e) => secrets_error_response(e),
    }
}

#[derive(Deserialize)]
struct RenameBody {
    new_name: String,
}

async fn rename_credential(
    State(state): State<SecretsApiState>,
    Path((provider, credential)): Path<(String, String)>,
    Json(body): Json<RenameBody>,
) -> (StatusCode, Json<Value>) {
    let provider = match parse_provider(&provider) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    match state.manager.rename(provider, &credential, &body.new_name) {
        Ok(status) => (
            StatusCode::OK,
            Json(serde_json::to_value(status).expect("CredentialStatus serializes")),
        ),
        Err(SecretsError::KeyStore(e)) if e.message.contains("credential not found") => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "credential_not_found", "credential": credential })),
        ),
        Err(e) => secrets_error_response(e),
    }
}

// ── helpers ────────────────────────────────────────────────────────────

fn parse_provider(s: &str) -> Result<ProviderId, (StatusCode, Json<Value>)> {
    s.parse::<ProviderId>().map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "unknown_provider", "provider": s })),
        )
    })
}

/// Map a [`SecretsError`] to an HTTP response. `KeyStoreError` is treated
/// as a backend failure (502) unless it carries the marker message used
/// by `rename` for "credential not found" — that case is handled in the
/// rename handler before we reach here.
fn secrets_error_response(e: SecretsError) -> (StatusCode, Json<Value>) {
    match e {
        SecretsError::InvalidCredentialName(name) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_credential_name", "name": name })),
        ),
        SecretsError::InvalidKey(reason) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_key", "reason": reason })),
        ),
        SecretsError::DefaultCredentialReserved => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "default_credential_reserved" })),
        ),
        SecretsError::CredentialExists(name) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": "credential_exists", "name": name })),
        ),
        SecretsError::KeyStore(KeyStoreError { account, message }) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "keystore_error", "account": account, "message": message })),
        ),
        SecretsError::Index { path, message } => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": "credential_index_error",
                "path": path.display().to_string(),
                "message": message,
            })),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderId;
    use crate::secrets::{InMemoryKeyStore, SecretsManager, StaticEnv};
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, StatusCode};
    use serde_json::Value;
    use tempfile::TempDir;
    use tower::ServiceExt;

    struct Harness {
        app: Router,
        manager: Arc<SecretsManager>,
        _dir: TempDir,
    }

    fn harness_with_env(env: StaticEnv) -> Harness {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("credentials.json");
        let manager = Arc::new(SecretsManager::new(
            Box::new(InMemoryKeyStore::new()),
            Box::new(env),
            path,
        ));
        let app = router(SecretsApiState::new(manager.clone()));
        Harness {
            app,
            manager,
            _dir: dir,
        }
    }

    fn harness() -> Harness {
        harness_with_env(StaticEnv::new())
    }

    async fn send(
        app: Router,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&b).unwrap())
            }
            None => Body::empty(),
        };
        let res = app.oneshot(req.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 64 * 1024).await.unwrap();
        let json: Value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, json)
    }

    // ── status ────────────────────────────────────────────────────

    #[tokio::test]
    async fn status_lists_all_categories_with_default_unconfigured() {
        let h = harness();
        let (status, body) = send(h.app, Method::GET, "/api/secrets/status", None).await;
        assert_eq!(status, StatusCode::OK);
        let cats = body.get("categories").unwrap().as_object().unwrap();
        assert!(cats.contains_key("stt"));
        assert!(cats.contains_key("llm"));
        assert!(cats.contains_key("tts"));
        let llm = cats.get("llm").unwrap().as_array().unwrap();
        let anthropic = llm.iter().find(|p| p["id"] == "anthropic").unwrap();
        let creds = anthropic["credentials"].as_array().unwrap();
        assert_eq!(creds[0]["name"], "default");
        assert_eq!(creds[0]["configured"], false);
    }

    // ── PUT ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn put_default_marks_keystore_configured() {
        let h = harness();
        let (status, body) = send(
            h.app.clone(),
            Method::PUT,
            "/api/secrets/anthropic/default",
            Some(json!({ "key": "sk-ant-12345" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["configured"], true);
        assert_eq!(body["source"], "keystore");
        assert_eq!(
            h.manager.resolve(ProviderId::Anthropic, "default"),
            Some("sk-ant-12345".into())
        );
    }

    #[tokio::test]
    async fn put_named_credential_adds_row() {
        let h = harness();
        let (s, _) = send(
            h.app.clone(),
            Method::PUT,
            "/api/secrets/anthropic/work",
            Some(json!({ "key": "sk-work" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (_, body) = send(h.app, Method::GET, "/api/secrets/status", None).await;
        let creds = body["categories"]["llm"][0]["credentials"]
            .as_array()
            .unwrap();
        assert!(creds.iter().any(|c| c["name"] == "work"));
    }

    #[tokio::test]
    async fn put_with_empty_key_returns_400() {
        let h = harness();
        let (status, body) = send(
            h.app,
            Method::PUT,
            "/api/secrets/anthropic/default",
            Some(json!({ "key": "" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_key");
    }

    #[tokio::test]
    async fn put_with_oversize_key_returns_400() {
        let h = harness();
        let huge = "a".repeat(2000);
        let (status, _) = send(
            h.app,
            Method::PUT,
            "/api/secrets/anthropic/default",
            Some(json!({ "key": huge })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn put_unknown_provider_returns_404() {
        let h = harness();
        let (status, body) = send(
            h.app,
            Method::PUT,
            "/api/secrets/openai/default",
            Some(json!({ "key": "k" })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "unknown_provider");
    }

    #[tokio::test]
    async fn put_invalid_credential_name_returns_400() {
        let h = harness();
        let (status, body) = send(
            h.app,
            Method::PUT,
            "/api/secrets/anthropic/Default",
            Some(json!({ "key": "k" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_credential_name");
    }

    // ── DELETE ────────────────────────────────────────────────────

    #[tokio::test]
    async fn delete_named_credential_removes_row() {
        let h = harness();
        send(
            h.app.clone(),
            Method::PUT,
            "/api/secrets/anthropic/work",
            Some(json!({ "key": "k" })),
        )
        .await;
        let (s, _) = send(
            h.app.clone(),
            Method::DELETE,
            "/api/secrets/anthropic/work",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (_, body) = send(h.app, Method::GET, "/api/secrets/status", None).await;
        let creds = body["categories"]["llm"][0]["credentials"]
            .as_array()
            .unwrap();
        assert!(!creds.iter().any(|c| c["name"] == "work"));
    }

    #[tokio::test]
    async fn delete_default_keeps_row_unconfigured() {
        let h = harness();
        send(
            h.app.clone(),
            Method::PUT,
            "/api/secrets/anthropic/default",
            Some(json!({ "key": "k" })),
        )
        .await;
        let (s, body) = send(
            h.app.clone(),
            Method::DELETE,
            "/api/secrets/anthropic/default",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body["configured"], false);
        let (_, status_body) = send(h.app, Method::GET, "/api/secrets/status", None).await;
        let creds = status_body["categories"]["llm"][0]["credentials"]
            .as_array()
            .unwrap();
        assert_eq!(creds[0]["name"], "default");
    }

    // ── rename ────────────────────────────────────────────────────

    #[tokio::test]
    async fn rename_moves_credential() {
        let h = harness();
        send(
            h.app.clone(),
            Method::PUT,
            "/api/secrets/anthropic/work",
            Some(json!({ "key": "kw" })),
        )
        .await;
        let (s, body) = send(
            h.app.clone(),
            Method::POST,
            "/api/secrets/anthropic/work/rename",
            Some(json!({ "new_name": "personal" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body["name"], "personal");
        assert_eq!(
            h.manager.resolve(ProviderId::Anthropic, "personal"),
            Some("kw".into())
        );
        assert_eq!(h.manager.resolve(ProviderId::Anthropic, "work"), None);
    }

    #[tokio::test]
    async fn rename_default_returns_400() {
        let h = harness();
        let (s, body) = send(
            h.app,
            Method::POST,
            "/api/secrets/anthropic/default/rename",
            Some(json!({ "new_name": "x" })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "default_credential_reserved");
    }

    #[tokio::test]
    async fn rename_to_default_returns_400() {
        let h = harness();
        send(
            h.app.clone(),
            Method::PUT,
            "/api/secrets/anthropic/work",
            Some(json!({ "key": "k" })),
        )
        .await;
        let (s, _) = send(
            h.app,
            Method::POST,
            "/api/secrets/anthropic/work/rename",
            Some(json!({ "new_name": "default" })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn rename_to_existing_returns_409() {
        let h = harness();
        send(
            h.app.clone(),
            Method::PUT,
            "/api/secrets/anthropic/work",
            Some(json!({ "key": "kw" })),
        )
        .await;
        send(
            h.app.clone(),
            Method::PUT,
            "/api/secrets/anthropic/personal",
            Some(json!({ "key": "kp" })),
        )
        .await;
        let (s, body) = send(
            h.app,
            Method::POST,
            "/api/secrets/anthropic/work/rename",
            Some(json!({ "new_name": "personal" })),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert_eq!(body["error"], "credential_exists");
    }

    #[tokio::test]
    async fn rename_missing_returns_404() {
        let h = harness();
        let (s, body) = send(
            h.app,
            Method::POST,
            "/api/secrets/anthropic/ghost/rename",
            Some(json!({ "new_name": "spirit" })),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "credential_not_found");
    }

    // ── env-var precedence ────────────────────────────────────────

    #[tokio::test]
    async fn env_backed_default_reports_env_source() {
        let mut env = StaticEnv::new();
        env.set("PARLEY_ANTHROPIC_API_KEY", "from-env");
        let h = harness_with_env(env);
        let (_, body) = send(h.app, Method::GET, "/api/secrets/status", None).await;
        let default = &body["categories"]["llm"][0]["credentials"][0];
        assert_eq!(default["configured"], true);
        assert_eq!(default["source"], "env");
    }

    #[tokio::test]
    async fn env_backed_default_survives_delete() {
        let mut env = StaticEnv::new();
        env.set("PARLEY_ANTHROPIC_API_KEY", "from-env");
        let h = harness_with_env(env);
        // Set a keystore value too, then delete it; env still wins.
        send(
            h.app.clone(),
            Method::PUT,
            "/api/secrets/anthropic/default",
            Some(json!({ "key": "from-keystore" })),
        )
        .await;
        send(
            h.app.clone(),
            Method::DELETE,
            "/api/secrets/anthropic/default",
            None,
        )
        .await;
        assert_eq!(
            h.manager.resolve(ProviderId::Anthropic, "default"),
            Some("from-env".into())
        );
        let (_, body) = send(h.app, Method::GET, "/api/secrets/status", None).await;
        let default = &body["categories"]["llm"][0]["credentials"][0];
        assert_eq!(default["source"], "env");
    }

    // ── secret-leak audit ─────────────────────────────────────────

    #[tokio::test]
    async fn no_endpoint_returns_the_secret_string() {
        let secret = "DO-NOT-LEAK-ME-1234567890";
        let h = harness();

        // Set the key via PUT.
        let (_, put_body) = send(
            h.app.clone(),
            Method::PUT,
            "/api/secrets/anthropic/default",
            Some(json!({ "key": secret })),
        )
        .await;
        assert!(!put_body.to_string().contains(secret), "PUT leaked secret");

        // Read status.
        let (_, status_body) = send(h.app.clone(), Method::GET, "/api/secrets/status", None).await;
        assert!(
            !status_body.to_string().contains(secret),
            "GET status leaked secret"
        );

        // Delete.
        let (_, del_body) = send(
            h.app,
            Method::DELETE,
            "/api/secrets/anthropic/default",
            None,
        )
        .await;
        assert!(
            !del_body.to_string().contains(secret),
            "DELETE leaked secret"
        );
    }
}
