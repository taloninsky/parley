use axum::extract::State;
use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::post};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::CorsLayer;

mod conversation_api;
mod llm;
mod orchestrator;
mod providers;
mod registry;
mod secrets;
mod secrets_api;
mod session_store;
mod tts;

use providers::ProviderId;
use secrets::{DEFAULT_CREDENTIAL, SecretsManager};

const ASSEMBLYAI_TOKEN_URL: &str = "https://streaming.assemblyai.com/v3/token";
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";

/// Max retries for transient failures on the token endpoint.
const TOKEN_MAX_RETRIES: u32 = 3;
/// Delay between token fetch retries.
const TOKEN_RETRY_DELAY: Duration = Duration::from_millis(500);

/// Shared application state holding a reusable HTTP client and the
/// secrets manager used to resolve provider API keys for the
/// `/token` and `/format` endpoints.
#[derive(Clone)]
struct AppState {
    client: reqwest::Client,
    secrets: Arc<SecretsManager>,
}

/// JSON body returned when an upstream provider has no `default`
/// credential configured. Spec §6.
fn provider_not_configured(provider: ProviderId) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::PRECONDITION_FAILED,
        Json(serde_json::json!({
            "error": "provider_not_configured",
            "provider": provider.as_str(),
            "credential": DEFAULT_CREDENTIAL,
        })),
    )
}

async fn fetch_token(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let api_key = match state
        .secrets
        .resolve(ProviderId::AssemblyAi, DEFAULT_CREDENTIAL)
    {
        Some(key) => key,
        None => return provider_not_configured(ProviderId::AssemblyAi),
    };
    let client = &state.client;
    let url = format!("{}?expires_in_seconds=480", ASSEMBLYAI_TOKEN_URL);

    let mut last_err = String::new();
    for attempt in 0..TOKEN_MAX_RETRIES {
        if attempt > 0 {
            eprintln!("[proxy] Token fetch retry {attempt}/{TOKEN_MAX_RETRIES}");
            tokio::time::sleep(TOKEN_RETRY_DELAY).await;
        }

        let resp = match client
            .get(&url)
            .header("Authorization", &api_key)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("{e:#}");
                eprintln!("[proxy] Token fetch attempt {attempt} transport error: {last_err}");
                continue;
            }
        };

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        if status.as_u16() == 429 || status.is_server_error() {
            last_err = format!("AssemblyAI HTTP {status}: {text}");
            eprintln!("[proxy] Token fetch attempt {attempt} retryable: {last_err}");
            continue;
        }

        if !status.is_success() {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": format!("AssemblyAI HTTP {status}: {text}")})),
            );
        }

        // Parse and forward the token
        return match serde_json::from_str::<serde_json::Value>(&text) {
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
        };
    }

    // All retries exhausted
    (
        StatusCode::BAD_GATEWAY,
        Json(
            serde_json::json!({"error": format!("upstream request failed after {TOKEN_MAX_RETRIES} attempts: {last_err}")}),
        ),
    )
}

// ── Formatting detection via Claude Haiku ────────────────────────────

#[derive(Deserialize)]
struct FormatRequest {
    /// Read-only context paragraphs (may be empty).
    #[serde(default)]
    context: String,
    /// The editable section Haiku is allowed to reformat.
    text: String,
    /// When true, add multi-speaker paragraph rules to the prompt.
    #[serde(default)]
    multi_speaker: bool,
    /// Anthropic model ID. Defaults to Haiku 4.5 if omitted.
    #[serde(default = "default_model")]
    model: String,
}

fn default_model() -> String {
    "claude-haiku-4-5-20251001".to_string()
}

const FORMAT_SYSTEM_PROMPT: &str = r#"You are a plain-text formatter for speech-to-text output.

You will receive a message in two sections:
- CONTEXT — already-formatted text for reference only. Do NOT modify or include
  in your output.
- EDITABLE — the recent text you may reformat.

Use CONTEXT only to judge whether the EDITABLE text continues the same topic
or starts a new one.

What you fix:
1. PUNCTUATION & CAPITALIZATION — add or remove commas, periods, question
   marks, exclamation marks, semicolons, colons, and em-dashes when context
   makes the correct punctuation clear. Capitalize the first word of each
   sentence (after a period, question mark, or exclamation mark).
2. PARAGRAPH BREAKS — when the speaker shifts to a clearly different topic,
   separate with a blank line.
3. ACRONYMS & ALPHANUMERIC IDENTIFIERS — when the STT has split a single
   acronym or identifier into separate letter/digit tokens, join them.
   Examples: "U S A" → "USA", "F B I" → "FBI", "G P T 4" → "GPT-4",
   "A B 1 2 3" → "AB123". Only join when the surrounding context makes it
   unambiguous that the speaker said an acronym/identifier rather than a
   sequence of individual letters or numbers.

=== ABSOLUTE RULE — NEVER CHANGE WORDS ===
The speaker's words are sacred. With the single exception of acronym/
identifier joining described in rule 3 above, you must NEVER add, remove,
substitute, or reorder any word. Every word in your output must appear in
the original text in the same order. If you are unsure whether a change
would alter a word, do NOT make that change.

What you MAY change:
- Punctuation (per rule 1).
- Capitalization, ONLY at the start of a sentence. Never capitalize a word
  mid-sentence.
- Whitespace: newlines, blank lines for paragraph breaks.
- Joining single-letter/digit tokens into acronyms or identifiers (per rule 3).

What is FORBIDDEN:
- Adding any word that was not in the original (other than via acronym joining).
- Removing any word (other than absorbing letter/digit tokens into an acronym).
- Replacing one word with another (e.g. "their" → "there").
- Reordering words.
- Bulleted or numbered list formatting (do not add "- " or "1. " prefixes,
  even if the speaker is enumerating).

If your output violates any of these, it will be automatically rejected.

- Return ONLY a JSON object, nothing else.
- If no changes are needed: {"changed": false}
- If changes are needed:  {"changed": true, "formatted": "..."}
  where "formatted" contains the full EDITABLE text with formatting applied.
  Do NOT include the CONTEXT in your output.

=== JSON ESCAPING — CRITICAL ===
The value of "formatted" is a JSON string. ALL control characters inside it
MUST be escaped, including paragraph breaks. Specifically:
- Encode every newline as the two-character sequence \n (backslash + n).
- Encode every carriage return as \r and every tab as \t.
- Encode every literal double-quote as \".
- Encode every literal backslash as \\.
Do NOT emit a raw newline character inside the JSON string value. A response
with a literal newline inside "formatted" is malformed JSON and will be
rejected. Example of a CORRECT response with a paragraph break:
  {"changed": true, "formatted": "First paragraph.\n\nSecond paragraph."}"#;

async fn format_text(
    State(state): State<Arc<AppState>>,
    Json(body): Json<FormatRequest>,
) -> impl IntoResponse {
    let api_key = match state
        .secrets
        .resolve(ProviderId::Anthropic, DEFAULT_CREDENTIAL)
    {
        Some(key) => key,
        None => return provider_not_configured(ProviderId::Anthropic),
    };
    let client = &state.client;

    // Build user message with optional context section
    let user_msg = if body.context.is_empty() {
        format!("EDITABLE:\n{}", body.text)
    } else {
        format!("CONTEXT:\n{}\n\nEDITABLE:\n{}", body.context, body.text)
    };

    let system_prompt = if body.multi_speaker {
        format!(
            "{}\n\n\
            ADDITIONAL RULES FOR MULTI-SPEAKER DIALOG:\n\
            This transcript is a DIALOG between two or more people, not a monolog.\n\
            Each paragraph may begin with structural markers that you must preserve exactly:\n\
            - Speaker tags: [Name] — identifies who is speaking (e.g. [Gavin], [Dave]).\n\
            - Timestamps: [MM:SS] or [H:MM:SS] — when the turn was spoken.\n\
            A typical line looks like: [05:23] [Gavin] So I was thinking about the architecture…\n\
            Or without timestamps: [Dave] Yeah, that makes sense.\n\n\
            CRITICAL: Speaker tags appear ONLY at speaker transitions — the first time a different person speaks.\n\
            Consecutive text from the same speaker does NOT repeat the [Name] tag. For example:\n\
            [Gavin] First point about the architecture. And here is a follow-up thought.\n\
            NOT: [Gavin] First point. [Gavin] And here is a follow-up.\n\n\
            Rules:\n\
            1. NEVER remove, rewrite, or reorder speaker tags or timestamps. They are structural, not content.\n\
            2. Each speaker tag MUST begin a new paragraph (preceded by a blank line). \
            Never merge text from different speakers into the same paragraph.\n\
            3. Consecutive turns from the SAME speaker join into one paragraph WITHOUT repeating the tag or timestamp.\n\
            4. If you add a paragraph break WITHIN a single speaker's text, do NOT add a [Name] tag \
            to the continuation paragraph — the speaker context carries implicitly until a new tag appears.\n\
            5. If you encounter redundant same-speaker tags mid-paragraph (e.g. [Gavin] text [Gavin] more text), \
            remove the duplicate tag so only the first one remains.\n\
            6. Apply paragraph breaks, punctuation, and formatting ONLY within a speaker's text — \
            the words after the [Name] tag.",
            FORMAT_SYSTEM_PROMPT
        )
    } else {
        FORMAT_SYSTEM_PROMPT.to_string()
    };

    let payload = serde_json::json!({
        "model": body.model,
        "max_tokens": 4096,
        "system": system_prompt,
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
        .header("x-api-key", &api_key)
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
            // Extract token usage from the Anthropic response
            let input_tokens = json["usage"]["input_tokens"].as_u64().unwrap_or(0);
            let output_tokens = json["usage"]["output_tokens"].as_u64().unwrap_or(0);

            let raw = json["content"]
                .as_array()
                .and_then(|arr| arr.first())
                .and_then(|block| block["text"].as_str())
                .unwrap_or("")
                .to_string();
            let full_json = format!("{{{raw}");
            // Parse Haiku's JSON — should be {"changed": bool, "formatted": "..."}.
            // Models occasionally emit literal newlines/tabs/CRs inside the
            // "formatted" string value, which is invalid JSON. Try a strict
            // parse first, and if that fails, repair control chars inside
            // string values and retry once before giving up.
            let parsed_result =
                serde_json::from_str::<serde_json::Value>(&full_json).or_else(|_| {
                    let repaired = repair_json_string_controls(&full_json);
                    serde_json::from_str::<serde_json::Value>(&repaired)
                });
            match parsed_result {
                Ok(parsed) => {
                    // Server-side validation: reject if Haiku changed any words
                    if let Some(formatted) = parsed["formatted"].as_str()
                        && !words_match(&body.text, formatted)
                    {
                        eprintln!("[proxy] Haiku changed words — rejecting response");
                        return (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "changed": false,
                                "input_tokens": input_tokens,
                                "output_tokens": output_tokens,
                            })),
                        );
                    }
                    // Merge token usage into the response
                    let mut result = parsed;
                    result["input_tokens"] = serde_json::json!(input_tokens);
                    result["output_tokens"] = serde_json::json!(output_tokens);
                    (StatusCode::OK, Json(result))
                }
                Err(_) => {
                    // Haiku returned invalid JSON — treat as no change
                    eprintln!("[proxy] Haiku returned invalid JSON: {full_json}");
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "changed": false,
                            "input_tokens": input_tokens,
                            "output_tokens": output_tokens,
                        })),
                    )
                }
            }
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": format!("parse error: {e}")})),
        ),
    }
}

/// Strip out bracketed structural markers (e.g. `[Gavin]`, `[05:23]`,
/// `[H:MM:SS]`) before word extraction. These are speaker tags and
/// timestamps used by the multi-speaker transcript format. They are
/// structural metadata, not transcribed words, so the validator must not
/// count them. Removing them up front lets the formatter legitimately
/// drop a duplicate `[Gavin]` mid-paragraph (per the multi-speaker prompt
/// rules) without being rejected for "changing words".
fn strip_bracketed_markers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth: u32 = 0;
    for c in text.chars() {
        if c == '[' {
            depth = depth.saturating_add(1);
        } else if c == ']' {
            if depth > 0 {
                depth -= 1;
            } else {
                // Unmatched ']' — treat as a regular character.
                out.push(c);
            }
        } else if depth == 0 {
            out.push(c);
        }
    }
    out
}

/// Extract only the words from text, stripping all punctuation and lowercasing.
/// Used to verify Haiku didn't add, remove, or change any words.
fn extract_words(text: &str) -> Vec<String> {
    strip_bracketed_markers(text)
        .split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric() || *c == '\'')
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// Repair JSON that contains literal control characters (newline, carriage
/// return, tab) inside string values. Walks the text tracking string vs.
/// non-string state and the `\` escape state; inside a string, replaces
/// raw control chars with their escaped two-character forms. Outside
/// strings, characters are passed through unchanged.
///
/// This is a targeted repair for one specific failure mode of LLM JSON
/// emission — it does NOT attempt to fix arbitrary malformed JSON.
fn repair_json_string_controls(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    let mut in_string = false;
    let mut escaped = false;
    for c in s.chars() {
        if in_string {
            if escaped {
                out.push(c);
                escaped = false;
            } else if c == '\\' {
                out.push(c);
                escaped = true;
            } else if c == '"' {
                out.push(c);
                in_string = false;
            } else if c == '\n' {
                out.push_str("\\n");
            } else if c == '\r' {
                out.push_str("\\r");
            } else if c == '\t' {
                out.push_str("\\t");
            } else {
                out.push(c);
            }
        } else {
            if c == '"' {
                in_string = true;
            }
            out.push(c);
        }
    }
    out
}

/// Collapse runs of two or more consecutive single-character alphanumeric
/// tokens into a single concatenated token. This matches the prompt's
/// acronym/identifier joining carve-out: e.g. ["u", "s", "a"] → ["usa"],
/// ["g", "p", "t", "4"] → ["gpt4"]. Single isolated single-char tokens
/// (e.g. the word "I" or "a") are left alone, so a stray drop of one of
/// them still fails the comparison. Applied to both sides of `words_match`.
fn collapse_acronym_runs(words: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(words.len());
    let mut run: Vec<String> = Vec::new();
    let flush = |run: &mut Vec<String>, out: &mut Vec<String>| {
        if run.len() >= 2 {
            out.push(run.concat());
        } else {
            out.append(run);
        }
        run.clear();
    };
    for w in words {
        if w.chars().count() == 1 && w.chars().next().unwrap().is_alphanumeric() {
            run.push(w);
        } else {
            flush(&mut run, &mut out);
            out.push(w);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// Compare two texts word-by-word (ignoring punctuation, whitespace, case).
/// Returns true if the words are identical in sequence after collapsing
/// any acronym runs (the one allowed word-shape change — see prompt rule 3).
fn words_match(original: &str, formatted: &str) -> bool {
    collapse_acronym_runs(extract_words(original))
        == collapse_acronym_runs(extract_words(formatted))
}

#[tokio::main]
async fn main() {
    let cors = CorsLayer::very_permissive();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("failed to build HTTP client");

    // Conversation Mode dependency: load persona / model registries from
    // ~/.parley/. Loaders are non-fatal — missing dirs and bad files are
    // logged, the proxy still serves token + format. The orchestrator
    // (later phase) will consume these registries; for now they are just
    // wired and logged so misconfiguration shows up at boot rather than
    // at first turn.
    let parley_dir = parley_config_dir();
    let models_dir = parley_dir.join("models");
    let personas_dir = parley_dir.join("personas");
    let prompts_dir = parley_dir.join("prompts");
    let models = registry::load_model_configs(&models_dir);
    let personas = registry::load_personas(&personas_dir, &prompts_dir, &models.entries);
    println!(
        "Loaded {} model config(s) and {} persona(s) from {}",
        models.count(),
        personas.count(),
        parley_dir.display(),
    );
    for err in models.errors.iter().chain(personas.errors.iter()) {
        eprintln!("[parley-config] {err}");
    }

    // Conversation API: shared in-process orchestrator handle, lazily
    // populated by `POST /conversation/init`. Holding the registries
    // behind `Arc` lets every request resolve persona / model without
    // re-reading from disk.
    let registries = Arc::new(conversation_api::Registries {
        personas: personas.entries,
        models: models.entries,
        prompts_dir: prompts_dir.clone(),
        sessions_dir: parley_dir.join("sessions"),
    });

    // Secrets manager: backed by the OS keystore in production, env-var
    // override for the `default` credential, and a small JSON index file
    // at `~/.parley/credentials.json` recording which named credentials
    // exist (the `keyring` crate has no portable enumeration API).
    let secrets_manager = Arc::new(secrets::SecretsManager::new(
        Box::new(secrets::KeyringStore::new()),
        Box::new(secrets::ProcessEnv),
        parley_dir.join("credentials.json"),
    ));

    let state = Arc::new(AppState {
        client: client.clone(),
        secrets: secrets_manager.clone(),
    });
    let conversation_state =
        conversation_api::ConversationApiState::new(registries, client, secrets_manager.clone());
    let secrets_state = secrets_api::SecretsApiState::new(secrets_manager);

    let app = Router::new()
        .route("/token", post(fetch_token))
        .route("/format", post(format_text))
        .with_state(state)
        .merge(conversation_api::router(conversation_state))
        .merge(secrets_api::router(secrets_state))
        .layer(cors);

    let addr = "127.0.0.1:3033";
    println!("Parley token proxy listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

/// Resolve the Parley config directory (`~/.parley` on Unix,
/// `%USERPROFILE%\.parley` on Windows). Falls back to the current
/// directory if no home dir is discoverable, which keeps the proxy
/// runnable in odd environments without crashing.
fn parley_config_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        PathBuf::from(home).join(".parley")
    } else {
        PathBuf::from(".parley")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_words ──────────────────────────────────────────────

    #[test]
    fn extract_words_strips_punctuation_and_lowercases() {
        assert_eq!(
            extract_words("Hello, World! It's GREAT."),
            vec!["hello", "world", "it's", "great"],
        );
    }

    #[test]
    fn extract_words_handles_empty_and_whitespace() {
        assert!(extract_words("").is_empty());
        assert!(extract_words("   \n\t  ").is_empty());
    }

    // ── collapse_acronym_runs ──────────────────────────────────────

    #[test]
    fn collapse_run_of_letters_joins_into_acronym() {
        let input = vec!["u".into(), "s".into(), "a".into()];
        assert_eq!(collapse_acronym_runs(input), vec!["usa"]);
    }

    #[test]
    fn collapse_mixed_letter_digit_run_joins_into_identifier() {
        let input = vec!["g".into(), "p".into(), "t".into(), "4".into()];
        assert_eq!(collapse_acronym_runs(input), vec!["gpt4"]);
    }

    #[test]
    fn collapse_isolated_single_char_token_left_alone() {
        // The English word "I" is a single-char token but shouldn't fuse
        // with its multi-char neighbours.
        let input = vec!["i".into(), "am".into(), "home".into()];
        assert_eq!(collapse_acronym_runs(input), vec!["i", "am", "home"]);
    }

    #[test]
    fn collapse_only_runs_of_two_or_more_are_joined() {
        // "i a" both single-char and consecutive → joined.
        let input = vec!["i".into(), "a".into(), "house".into()];
        assert_eq!(collapse_acronym_runs(input), vec!["ia", "house"]);
    }

    #[test]
    fn collapse_handles_multiple_separated_runs() {
        let input = vec![
            "the".into(),
            "f".into(),
            "b".into(),
            "i".into(),
            "raided".into(),
            "g".into(),
            "p".into(),
            "t".into(),
        ];
        assert_eq!(
            collapse_acronym_runs(input),
            vec!["the", "fbi", "raided", "gpt"],
        );
    }

    #[test]
    fn collapse_run_at_start_and_end() {
        let input = vec![
            "u".into(),
            "s".into(),
            "wins".into(),
            "g".into(),
            "o".into(),
        ];
        assert_eq!(collapse_acronym_runs(input), vec!["us", "wins", "go"]);
    }

    #[test]
    fn collapse_empty_input_yields_empty_output() {
        assert!(collapse_acronym_runs(Vec::new()).is_empty());
    }

    // ── words_match ────────────────────────────────────────────────

    #[test]
    fn words_match_identical_text() {
        assert!(words_match("Hello world.", "Hello world."));
    }

    #[test]
    fn words_match_punctuation_and_case_differences_allowed() {
        assert!(words_match(
            "hello world it works",
            "Hello, world — it works!",
        ));
    }

    #[test]
    fn words_match_acronym_join_allowed() {
        // The carve-out: STT split "USA" into single letters, formatter joined.
        assert!(words_match("the u s a is great", "the USA is great"));
    }

    #[test]
    fn words_match_alphanumeric_identifier_join_allowed() {
        assert!(words_match(
            "we use g p t 4 for this",
            "we use GPT4 for this",
        ));
    }

    #[test]
    fn words_match_dropped_word_rejected() {
        assert!(!words_match("hello big world", "hello world"));
    }

    #[test]
    fn words_match_substituted_word_rejected() {
        assert!(!words_match("their car is fast", "there car is fast"));
    }

    #[test]
    fn words_match_added_word_rejected() {
        assert!(!words_match("hello world", "hello cruel world"));
    }

    #[test]
    fn words_match_reordered_words_rejected() {
        assert!(!words_match("the quick fox", "the fox quick"));
    }

    #[test]
    fn words_match_dropped_letter_inside_acronym_run_rejected() {
        // Original "U S A B" → if formatter drops "B" and outputs "USA",
        // the run collapses to "usa" vs "usab" — must fail.
        assert!(!words_match("u s a b context", "USA context"));
    }

    // ── strip_bracketed_markers ────────────────────────────────────

    #[test]
    fn strip_brackets_removes_speaker_tag() {
        assert_eq!(strip_bracketed_markers("[Gavin] hello"), " hello");
    }

    #[test]
    fn strip_brackets_removes_timestamp_and_tag() {
        assert_eq!(strip_bracketed_markers("[05:23] [Dave] yeah"), "  yeah",);
    }

    #[test]
    fn strip_brackets_handles_unmatched_close_bracket() {
        // Stray ']' with no opener is preserved as a literal character so
        // we don't silently mangle unrelated text.
        assert_eq!(strip_bracketed_markers("a] b"), "a] b");
    }

    // ── words_match with multi-speaker markers ─────────────────────

    #[test]
    fn words_match_ignores_speaker_tags() {
        // The validator must not count "[Gavin]" as a word containing "gavin".
        assert!(words_match("[Gavin] hello world", "[Gavin] Hello, world.",));
    }

    #[test]
    fn words_match_allows_dropping_redundant_speaker_tag() {
        // Multi-speaker rule 5: a duplicate same-speaker tag mid-paragraph
        // may be removed by the formatter without that counting as a word
        // change.
        assert!(words_match(
            "[Gavin] first point [Gavin] follow up",
            "[Gavin] First point, follow up.",
        ));
    }

    #[test]
    fn words_match_allows_dropping_timestamp_marker() {
        assert!(words_match(
            "[05:23] [Gavin] hello there",
            "[Gavin] Hello there.",
        ));
    }

    // ── repair_json_string_controls ────────────────────────────────

    #[test]
    fn repair_escapes_literal_newline_inside_string_value() {
        let bad = "{\"changed\":true,\"formatted\":\"para one\n\npara two\"}";
        let repaired = repair_json_string_controls(bad);
        let parsed: serde_json::Value =
            serde_json::from_str(&repaired).expect("repaired JSON should parse");
        assert_eq!(parsed["changed"], true);
        assert_eq!(parsed["formatted"], "para one\n\npara two");
    }

    #[test]
    fn repair_escapes_literal_tab_and_carriage_return() {
        let bad = "{\"formatted\":\"a\tb\rc\"}";
        let repaired = repair_json_string_controls(bad);
        let parsed: serde_json::Value =
            serde_json::from_str(&repaired).expect("repaired JSON should parse");
        assert_eq!(parsed["formatted"], "a\tb\rc");
    }

    #[test]
    fn repair_preserves_already_escaped_sequences() {
        // Already-valid JSON should round-trip identically.
        let good = "{\"formatted\":\"line one\\nline two\"}";
        let repaired = repair_json_string_controls(good);
        assert_eq!(repaired, good);
    }

    #[test]
    fn repair_does_not_touch_whitespace_outside_strings() {
        // Newlines between fields in pretty-printed JSON are valid; leave them.
        let good = "{\n  \"changed\": false\n}";
        let repaired = repair_json_string_controls(good);
        assert_eq!(repaired, good);
    }

    #[test]
    fn repair_handles_escaped_quote_inside_string() {
        // The string contains an escaped quote followed by a literal newline
        // — the escaped quote must NOT terminate the string for repair purposes.
        let bad = "{\"formatted\":\"she said \\\"hi\\\"\nthen left\"}";
        let repaired = repair_json_string_controls(bad);
        let parsed: serde_json::Value =
            serde_json::from_str(&repaired).expect("repaired JSON should parse");
        assert_eq!(parsed["formatted"], "she said \"hi\"\nthen left");
    }
}
