//! Browser-side client for the proxy's `/api/secrets` HTTP surface.
//!
//! Mirrors the wire types defined in [`proxy::secrets_api`] so the UI
//! can render the categorized status response and round-trip
//! credential mutations without touching the OS keystore directly
//! (only the proxy holds those keys).
//!
//! ## Usage
//!
//! - [`use_secrets_status`] returns a `(status, refresh)` pair: a
//!   signal that tracks the latest fetched status, and a closure that
//!   re-runs the fetch. Components should call `refresh()` after any
//!   `set_credential` / `delete_credential` / `rename_credential`
//!   completes so the UI reflects the new state.
//! - [`set_credential`], [`delete_credential`], [`rename_credential`]
//!   are thin async wrappers around the four HTTP routes.
//!
//! ## Security
//!
//! Keys are write-only on the wire: requests carry the key as a
//! request body, responses never include it. A successful PUT
//! returns the new [`CredentialStatus`] (configured / source /
//! warning) so the UI can update without a separate fetch.
//!
//! Spec: `docs/secrets-storage-spec.md` §5.

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

const API_BASE: &str = "http://127.0.0.1:3033/api/secrets";

/// Where a configured credential's value comes from. `Env` means the
/// `PARLEY_<PROVIDER>_API_KEY` environment variable shadows whatever
/// is in the keystore for the `default` credential; the UI surfaces
/// this so users understand why "Remove" is disabled in that case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialSource {
    /// Value resolved from a process environment variable.
    Env,
    /// Value resolved from the OS keystore.
    Keystore,
}

/// Status of a single named credential under one provider.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CredentialStatus {
    /// Credential name (`default` or a user-chosen `[a-z0-9_-]{1,32}`
    /// label).
    pub name: String,
    /// `true` when a value is resolvable for this credential —
    /// either an env var (default only) or a keystore entry.
    pub configured: bool,
    /// Where the value came from when `configured` is `true`. Absent
    /// when the credential has no value.
    #[serde(default)]
    pub source: Option<CredentialSource>,
    /// Optional human-readable warning. Today this is set when env
    /// var and keystore both have entries for the `default` slot to
    /// flag the precedence to the user.
    #[serde(default)]
    pub warning: Option<String>,
}

/// Status of a provider plus all of its named credentials.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderStatus {
    /// Provider id (e.g. `anthropic`, `assemblyai`).
    pub id: String,
    /// Human-readable provider label for the UI.
    pub display_name: String,
    /// Name of the env var consulted for the `default` credential.
    pub env_var: String,
    /// All credentials known for this provider, including `default`
    /// (which is always present even when unconfigured).
    pub credentials: Vec<CredentialStatus>,
}

/// Top-level status payload returned by `GET /api/secrets/status`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SecretsStatus {
    /// Providers grouped by category (`stt`, `llm`, `tts`).
    pub categories: BTreeMap<String, Vec<ProviderStatus>>,
    /// Non-fatal startup errors surfaced by the proxy (e.g. an
    /// unreadable credential index file). Empty in steady state.
    #[serde(default)]
    pub errors: Vec<String>,
}

impl SecretsStatus {
    /// Look up a provider by id across all categories. Returns
    /// `None` if the id is unknown to the proxy registry.
    pub fn provider(&self, id: &str) -> Option<&ProviderStatus> {
        self.categories
            .values()
            .flat_map(|provs| provs.iter())
            .find(|p| p.id == id)
    }

    /// Convenience: list the credential names for a provider, in the
    /// order returned by the proxy (`default` first, then named
    /// credentials sorted lexicographically).
    #[allow(dead_code)] // used by conversation.rs credential dropdown
    pub fn credential_names(&self, provider_id: &str) -> Vec<String> {
        self.provider(provider_id)
            .map(|p| p.credentials.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default()
    }
}

impl ProviderStatus {
    /// Find a credential by name.
    pub fn credential(&self, name: &str) -> Option<&CredentialStatus> {
        self.credentials.iter().find(|c| c.name == name)
    }
}

/// Refresh handle returned by [`use_secrets_status`]. Calling
/// `refresh()` re-fetches the status from the proxy and updates the
/// associated signal.
#[derive(Clone, Copy)]
pub struct SecretsRefresh {
    bump: Signal<u64>,
}

impl SecretsRefresh {
    /// Trigger a re-fetch of the status payload. Cheap; the actual
    /// HTTP call runs in the background.
    pub fn refresh(&mut self) {
        let v = *self.bump.peek();
        self.bump.set(v.wrapping_add(1));
    }
}

/// Hook: subscribe to the proxy's secrets status. Returns a resource
/// that resolves to `Result<SecretsStatus, String>` and a refresh
/// handle to re-fire the fetch after a mutation.
///
/// The resource re-runs whenever `refresh()` is called on the
/// returned handle. On the very first render it kicks off
/// immediately.
pub fn use_secrets_status() -> (Resource<Result<SecretsStatus, String>>, SecretsRefresh) {
    let bump = use_signal(|| 0u64);
    let resource = use_resource(move || async move {
        // Subscribe so refresh() bumps re-trigger the fetch.
        let _ = bump();
        fetch_status().await
    });
    (resource, SecretsRefresh { bump })
}

/// Fetch the status payload from the proxy. Returns the raw HTTP
/// error message on failure (e.g. proxy down) so callers can render
/// it directly.
async fn fetch_status() -> Result<SecretsStatus, String> {
    let window = web_sys::window().ok_or("no window")?;
    let opts = web_sys::RequestInit::new();
    opts.set_method("GET");
    let url = format!("{API_BASE}/status");
    let request =
        web_sys::Request::new_with_str_and_init(&url, &opts).map_err(|e| format!("{e:?}"))?;
    let resp_val = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|e| format!("fetch failed: {e:?}"))?;
    let resp: web_sys::Response = resp_val
        .dyn_into()
        .map_err(|_| "response cast failed".to_string())?;
    if !resp.ok() {
        return Err(format!("proxy returned HTTP {}", resp.status()));
    }
    let json_val = JsFuture::from(resp.json().map_err(|e| format!("{e:?}"))?)
        .await
        .map_err(|e| format!("json parse: {e:?}"))?;
    let json_str = js_sys::JSON::stringify(&json_val)
        .map_err(|e| format!("stringify: {e:?}"))?
        .as_string()
        .ok_or("stringify returned non-string")?;
    serde_json::from_str(&json_str).map_err(|e| format!("decode: {e}"))
}

/// `PUT /api/secrets/{provider}/{credential}` — set or replace a
/// credential's value. `key` travels in the request body as
/// `{"key": "..."}` and is never echoed back.
pub async fn set_credential(
    provider: &str,
    credential: &str,
    key: &str,
) -> Result<CredentialStatus, String> {
    let url = format!("{API_BASE}/{provider}/{credential}");
    let body = serde_json::json!({ "key": key }).to_string();
    request_json("PUT", &url, Some(&body)).await
}

/// `DELETE /api/secrets/{provider}/{credential}` — remove a
/// credential from the keystore. Idempotent: a 404 is treated as
/// success on the proxy side.
pub async fn delete_credential(provider: &str, credential: &str) -> Result<(), String> {
    let url = format!("{API_BASE}/{provider}/{credential}");
    let _: serde_json::Value = request_json("DELETE", &url, None).await?;
    Ok(())
}

/// `POST /api/secrets/{provider}/{credential}/rename` — rename a
/// named credential. The `default` credential is reserved and
/// cannot be either source or target.
#[allow(dead_code)] // wired in via the future categorized credentials UI
pub async fn rename_credential(
    provider: &str,
    from: &str,
    to: &str,
) -> Result<CredentialStatus, String> {
    let url = format!("{API_BASE}/{provider}/{from}/rename");
    let body = serde_json::json!({ "to": to }).to_string();
    request_json("POST", &url, Some(&body)).await
}

/// Shared HTTP helper for the JSON-bodied mutation endpoints. Maps
/// non-2xx responses to a string error built from the proxy's
/// `error` field when available, falling back to the HTTP status.
async fn request_json<T: for<'de> Deserialize<'de>>(
    method: &str,
    url: &str,
    body: Option<&str>,
) -> Result<T, String> {
    let window = web_sys::window().ok_or("no window")?;
    let opts = web_sys::RequestInit::new();
    opts.set_method(method);
    if let Some(body) = body {
        let headers = web_sys::Headers::new().map_err(|e| format!("{e:?}"))?;
        headers
            .set("Content-Type", "application/json")
            .map_err(|e| format!("{e:?}"))?;
        opts.set_headers(&headers);
        opts.set_body(&JsValue::from_str(body));
    }
    let request =
        web_sys::Request::new_with_str_and_init(url, &opts).map_err(|e| format!("{e:?}"))?;
    let resp_val = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|e| format!("fetch failed: {e:?}"))?;
    let resp: web_sys::Response = resp_val
        .dyn_into()
        .map_err(|_| "response cast failed".to_string())?;
    if !resp.ok() {
        // Try to surface the proxy's error message; fall back to status.
        let status = resp.status();
        let text = JsFuture::from(resp.text().map_err(|e| format!("{e:?}"))?)
            .await
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default();
        let msg = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or_else(|| format!("HTTP {status}: {text}"));
        return Err(msg);
    }
    let json_val = JsFuture::from(resp.json().map_err(|e| format!("{e:?}"))?)
        .await
        .map_err(|e| format!("json parse: {e:?}"))?;
    let json_str = js_sys::JSON::stringify(&json_val)
        .map_err(|e| format!("stringify: {e:?}"))?
        .as_string()
        .ok_or("stringify returned non-string")?;
    serde_json::from_str(&json_str).map_err(|e| format!("decode: {e}"))
}
