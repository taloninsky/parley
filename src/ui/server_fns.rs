use dioxus::prelude::*;

const ASSEMBLYAI_TOKEN_URL: &str = "https://api.assemblyai.com/v2/realtime/token";

/// Server function that fetches a temporary streaming token from AssemblyAI.
/// Runs server-side to avoid CORS issues with browser fetch.
#[server]
pub async fn fetch_streaming_token(api_key: String) -> Result<String, ServerFnError> {
    let client = reqwest::Client::new();
    let resp = client
        .post(ASSEMBLYAI_TOKEN_URL)
        .header("Authorization", &api_key)
        .header("Content-Type", "application/json")
        .body(r#"{"expires_in": 3600}"#)
        .send()
        .await
        .map_err(|e| ServerFnError::new(format!("request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(ServerFnError::new(format!(
            "AssemblyAI returned HTTP {status}: {body}"
        )));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| ServerFnError::new(format!("json parse failed: {e}")))?;

    json.get("token")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| ServerFnError::new("no token field in response".to_string()))
}
