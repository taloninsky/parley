use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::post};
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;

const ASSEMBLYAI_TOKEN_URL: &str = "https://streaming.assemblyai.com/v3/token";
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";

#[derive(Deserialize)]
struct TokenRequest {
    api_key: String,
}

async fn fetch_token(Json(body): Json<TokenRequest>) -> impl IntoResponse {
    let client = reqwest::Client::new();
    let resp = match client
        .get(format!("{}?expires_in_seconds=480", ASSEMBLYAI_TOKEN_URL))
        .header("Authorization", &body.api_key)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": format!("upstream request failed: {e}")})),
            );
        }
    };

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        return (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": format!("AssemblyAI HTTP {status}: {text}")})),
        );
    }

    // Parse and forward the token
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(json) => match json.get("token").and_then(|t| t.as_str()) {
            Some(token) => (StatusCode::OK, Json(serde_json::json!({"token": token}))),
            None => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": "no token in AssemblyAI response"})),
            ),
        },
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": format!("parse error: {e}")})),
        ),
    }
}

// ── Formatting detection via Claude Haiku ────────────────────────────

#[derive(Deserialize)]
struct FormatRequest {
    anthropic_key: String,
    /// Read-only context paragraphs (may be empty).
    #[serde(default)]
    context: String,
    /// The editable section Haiku is allowed to reformat.
    text: String,
}

const FORMAT_SYSTEM_PROMPT: &str = r#"You are a plain-text formatter for speech-to-text output.

You will receive a message in two sections:
- CONTEXT — already-formatted text for reference only. Do NOT modify or include
  in your output.
- EDITABLE — the recent text you may reformat.

A "chunk" is a paragraph optionally followed by a bulleted or numbered list
that belongs to it. Use CONTEXT only to judge whether the EDITABLE text
continues the same topic or starts a new one.

Patterns to detect (apply whichever fit):
1. PARAGRAPH BREAKS — when the speaker shifts to a clearly different topic,
   separate with a blank line.
2. BULLETED LISTS — items enumerated informally ("things like X, Y, Z" or
   "first... second...") → lines starting with "- ".  Keep the list
   attached to the paragraph above it (no blank line between them).
3. NUMBERED LISTS — explicitly numbered items ("number one... number two...")
   → "1. ", "2. ", etc.  Keep attached to the paragraph above.

=== ABSOLUTE RULE — NEVER CHANGE WORDS ===
The speaker's words are sacred. You must NEVER add, remove, substitute, or
reorder any word. Every single word in your output must appear in the original
text in exactly the same order. If you are unsure whether a change would alter
a word, do NOT make that change.

What you MAY change:
- Punctuation: add or remove commas, periods, question marks, exclamation
  marks, semicolons, colons, and em-dashes when context makes the correct
  punctuation clear.
- Capitalization: ONLY at the very start of a sentence (after a period,
  question mark, or exclamation mark). Never capitalize a word mid-sentence.
- Whitespace: newlines, blank lines, leading "- " or "1. " for lists.
- You may merge short chunks back together if they clearly belong to the
  same thought.

Reminder — the following are FORBIDDEN:
- Adding a word that was not in the original.
- Removing a word that was in the original.
- Replacing one word with another (e.g. "their" → "there").
- Reordering words.
If your output violates any of these, it will be automatically rejected.

- Return ONLY a JSON object, nothing else.
- If no changes are needed: {"changed": false}
- If changes are needed:  {"changed": true, "formatted": "..."}
  where "formatted" contains the full EDITABLE text with formatting applied.
  Do NOT include the CONTEXT in your output."#;

async fn format_text(Json(body): Json<FormatRequest>) -> impl IntoResponse {
    let client = reqwest::Client::new();

    // Build user message with optional context section
    let user_msg = if body.context.is_empty() {
        format!("EDITABLE:\n{}", body.text)
    } else {
        format!("CONTEXT:\n{}\n\nEDITABLE:\n{}", body.context, body.text)
    };

    let payload = serde_json::json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 4096,
        "system": FORMAT_SYSTEM_PROMPT,
        "messages": [
            {
                "role": "user",
                "content": user_msg,
            },
            {
                "role": "assistant",
                "content": "{",
            }
        ]
    });

    let resp = match client
        .post(ANTHROPIC_MESSAGES_URL)
        .header("x-api-key", &body.anthropic_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": format!("Anthropic request failed: {e}")})),
            );
        }
    };

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        eprintln!("[proxy] Anthropic HTTP {status}: {text}");
        return (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": format!("Anthropic HTTP {status}: {text}")})),
        );
    }

    // Extract Haiku's response and prepend the prefilled "{"
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(json) => {
            let raw = json["content"]
                .as_array()
                .and_then(|arr| arr.first())
                .and_then(|block| block["text"].as_str())
                .unwrap_or("")
                .to_string();
            let full_json = format!("{{{raw}");
            // Parse Haiku's JSON — should be {"changed": bool, "formatted": "..."}
            match serde_json::from_str::<serde_json::Value>(&full_json) {
                Ok(parsed) => {
                    // Server-side validation: reject if Haiku changed any words
                    if let Some(formatted) = parsed["formatted"].as_str() {
                        if !words_match(&body.text, formatted) {
                            eprintln!("[proxy] Haiku changed words — rejecting response");
                            return (StatusCode::OK, Json(serde_json::json!({"changed": false})));
                        }
                    }
                    (StatusCode::OK, Json(parsed))
                }
                Err(_) => {
                    // Haiku returned invalid JSON — treat as no change
                    eprintln!("[proxy] Haiku returned invalid JSON: {full_json}");
                    (StatusCode::OK, Json(serde_json::json!({"changed": false})))
                }
            }
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": format!("parse error: {e}")})),
        ),
    }
}

/// Extract only the words from text, stripping all punctuation and lowercasing.
/// Used to verify Haiku didn't add, remove, or change any words.
fn extract_words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric() || *c == '\'')
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// Compare two texts word-by-word (ignoring punctuation, whitespace, case).
/// Returns true if the words are identical in sequence.
fn words_match(original: &str, formatted: &str) -> bool {
    extract_words(original) == extract_words(formatted)
}

#[tokio::main]
async fn main() {
    let cors = CorsLayer::very_permissive();

    let app = Router::new()
        .route("/token", post(fetch_token))
        .route("/format", post(format_text))
        .layer(cors);

    let addr = "127.0.0.1:3033";
    println!("Parley token proxy listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
