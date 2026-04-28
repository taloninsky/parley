use axum::extract::State;
use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::post};
use parley_core::model_config::LlmProviderTag;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::CorsLayer;

mod conversation_api;
mod llm;
mod orchestrator;
mod profile;
mod providers;
mod registry;
mod secrets;
mod secrets_api;
mod session_store;
mod stt;
mod stt_api;
mod tts;
mod tts_api;

use providers::ProviderId;
use secrets::{DEFAULT_CREDENTIAL, SecretsManager};

const ASSEMBLYAI_TOKEN_URL: &str = "https://streaming.assemblyai.com/v3/token";
const SONIOX_TEMPORARY_API_KEY_URL: &str = "https://api.soniox.com/v1/auth/temporary-api-key";
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
    soniox_temporary_api_key_url: String,
    anthropic_messages_url: String,
    /// Registry view shared with the conversation API. The `/format`
    /// handler looks up `model_config_id` here to resolve the
    /// provider tag + raw model name. See
    /// `docs/global-reformat-spec.md` §2.
    registries: Arc<conversation_api::Registries>,
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

async fn fetch_soniox_token(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let api_key = match state
        .secrets
        .resolve(ProviderId::Soniox, DEFAULT_CREDENTIAL)
    {
        Some(key) => key,
        None => return provider_not_configured(ProviderId::Soniox),
    };
    let client = &state.client;

    let mut last_err = String::new();
    for attempt in 0..TOKEN_MAX_RETRIES {
        if attempt > 0 {
            eprintln!("[proxy] Soniox token fetch retry {attempt}/{TOKEN_MAX_RETRIES}");
            tokio::time::sleep(TOKEN_RETRY_DELAY).await;
        }

        let resp = match client
            .post(&state.soniox_temporary_api_key_url)
            .bearer_auth(&api_key)
            .json(&serde_json::json!({
                "usage_type": "transcribe_websocket",
                "expires_in_seconds": 480,
            }))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("{e:#}");
                eprintln!(
                    "[proxy] Soniox token fetch attempt {attempt} transport error: {last_err}"
                );
                continue;
            }
        };

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        if status.as_u16() == 429 || status.is_server_error() {
            last_err = format!("Soniox HTTP {status}: {text}");
            eprintln!("[proxy] Soniox token fetch attempt {attempt} retryable: {last_err}");
            continue;
        }

        if !status.is_success() {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": format!("Soniox HTTP {status}: {text}")})),
            );
        }

        return match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(json) => match json.get("api_key").and_then(|t| t.as_str()) {
                Some(api_key) => (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "api_key": api_key,
                        "expires_at": json.get("expires_at").cloned().unwrap_or(serde_json::Value::Null),
                    })),
                ),
                None => (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({"error": "no api_key in Soniox response"})),
                ),
            },
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": format!("parse error: {e}")})),
            ),
        };
    }

    (
        StatusCode::BAD_GATEWAY,
        Json(
            serde_json::json!({"error": format!("upstream request failed after {TOKEN_MAX_RETRIES} attempts: {last_err}")}),
        ),
    )
}

// ── Formatting detection via Claude Haiku ────────────────────────────

/// Request body for `POST /format`.
///
/// The new wire shape (spec `docs/global-reformat-spec.md` §2) carries
/// a `model_config_id` referencing one of the registries loaded at
/// boot, plus an optional `credential` name for multi-key setups.
///
/// `model` is preserved as a one-release deprecated alias: when only
/// `model` is sent it is treated as the raw Anthropic model name and
/// dispatched through the legacy Anthropic-only path with the
/// `default` credential. When `model_config_id` is present (even
/// alongside `model`) the registry path wins.
#[derive(Deserialize, Default)]
struct FormatRequest {
    /// Read-only context paragraphs (may be empty).
    #[serde(default)]
    context: String,
    /// The editable section the formatter is allowed to reformat.
    text: String,
    /// When true, add multi-speaker paragraph rules to the prompt.
    #[serde(default)]
    multi_speaker: bool,
    /// Registry id of the model to format with. Empty / missing falls
    /// back to `model` for backward compatibility.
    #[serde(default)]
    model_config_id: String,
    /// Named credential to draw the provider API key from. Defaults
    /// to `default` when omitted.
    #[serde(default)]
    credential: String,
    /// **Deprecated.** Raw Anthropic model name. Accepted for one
    /// release while clients migrate to `model_config_id`.
    #[serde(default)]
    model: String,
}

fn default_anthropic_model() -> String {
    "claude-haiku-4-5-20251001".to_string()
}

const FORMAT_SYSTEM_PROMPT: &str = r#"You are a plain-text formatter for speech-to-text output.

You will receive a message in two sections:
- CONTEXT — already-formatted text for reference only. Do NOT modify or include
  in your output.
- EDITABLE — the recent text you may reformat.

Use CONTEXT only to judge whether the EDITABLE text continues the same topic
or starts a new one.

Your primary job is to take run-on, lightly-punctuated speech-to-text output
and produce a readable plain-text transcript. Speech-to-text systems do NOT
emit good paragraph breaks, and they often miss commas, periods, and
question marks. You are expected to add them. Returning the input unchanged
("changed": false) is rare — it is only correct when the input is already
well-punctuated AND already broken into paragraphs at the appropriate
boundaries. If you are looking at a wall of text with multiple speaker
tags inline, multiple sentences run together, or topic shifts inside one
paragraph, the answer is ALWAYS "changed": true.

What you fix:
1. PARAGRAPH BREAKS — break the text into paragraphs at every clear
    boundary. Boundaries include speaker transitions (see multi-speaker
    rules below when present), the start of a new topic, a long pause, or
    a clear shift in subject. Paragraphing is semantic: split when the
    speaker moves from one subject of discussion to another, starts a new
    line of reasoning, changes from background to conclusion/action items,
    or uses transition phrases such as "moving on", "another thing", "the
    next thing", "on a different note", or "switching topics". Long
    monologues from a single speaker SHOULD be split into multiple
    paragraphs at topic boundaries. A single paragraph longer than ~5
    sentences is almost always wrong.
2. PUNCTUATION & CAPITALIZATION — add or remove commas, periods, question
   marks, exclamation marks, semicolons, colons, and em-dashes when context
   makes the correct punctuation clear. Capitalize the first word of each
   sentence (after a period, question mark, or exclamation mark).
3. ACRONYMS & ALPHANUMERIC IDENTIFIERS — when the STT has split a single
   acronym or identifier into separate letter/digit tokens, join them.
   Examples: "U S A" → "USA", "F B I" → "FBI", "G P T 4" → "GPT-4",
   "A B 1 2 3" → "AB123". Only join when the surrounding context makes it
   unambiguous that the speaker said an acronym/identifier rather than a
   sequence of individual letters or numbers.
4. SUBWORD MERGING — speech-to-text occasionally splits a single word into
   two pieces with a stray space (e.g. "speci es", "Encyclop edia",
   "Somet imes"). When the surrounding context makes the intended single
   word unambiguous, glue the pieces back together. The letter sequence
   does not change; only the whitespace inside the word is removed.

=== ABSOLUTE RULE — NEVER CHANGE WORDS ===
The speaker's words are sacred. With the explicit exceptions of
acronym/identifier joining (rule 3) and subword merging (rule 4), you must
NEVER add, remove, substitute, or reorder any word. Every word in your
output must appear in the original text in the same order. If you are
unsure whether a change would alter a word, do NOT make that change.

What you MAY change:
- Punctuation (per rule 2).
- Capitalization, ONLY at the start of a sentence. Never capitalize a word
  mid-sentence (proper noun capitalization stays as-is from the STT).
- Whitespace: newlines, blank lines for paragraph breaks. Inserting a
  paragraph break never counts as changing a word.
- Joining single-letter/digit tokens into acronyms or identifiers (rule 3).
- Merging a word that the STT split with a stray space (rule 4).

What is FORBIDDEN:
- Adding any word that was not in the original.
- Removing any word.
- Replacing one word with another (e.g. "their" → "there", "Yingsy" → "Yingst").
- Reordering words.
- Spelling a number as a word or vice versa ("5" ↔ "five").
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
    // Top-of-handler diagnostic so we can verify which proxy build is
    // serving the request and what shape the request actually has.
    eprintln!(
        "[proxy] /format request: text_len={}, context_len={}, \
         multi_speaker_flag={}, model_config_id={:?}, model={:?}, credential={:?}",
        body.text.len(),
        body.context.len(),
        body.multi_speaker,
        body.model_config_id,
        body.model,
        body.credential,
    );

    // Resolve provider, target model name, and credential from the
    // request. The new shape uses `model_config_id` to look up a
    // registry entry; the deprecated `model` alias still works for
    // one release. Spec §2.
    let credential_name = if body.credential.is_empty() {
        DEFAULT_CREDENTIAL.to_string()
    } else {
        body.credential.clone()
    };
    let (model_name, provider_label) = if !body.model_config_id.is_empty() {
        match state.registries.models.get(&body.model_config_id) {
            Some(cfg) => {
                if cfg.provider != LlmProviderTag::Anthropic {
                    let label = match cfg.provider {
                        LlmProviderTag::Anthropic => "anthropic",
                        LlmProviderTag::Openai => "openai",
                        LlmProviderTag::Google => "google",
                        LlmProviderTag::Xai => "xai",
                        LlmProviderTag::LocalOpenaiCompatible => "local",
                    };
                    return (
                        StatusCode::NOT_IMPLEMENTED,
                        Json(serde_json::json!({
                            "error": format!(
                                "provider {label} not yet supported by /format"
                            ),
                            "model_config_id": body.model_config_id,
                            "provider": label,
                        })),
                    );
                }
                (cfg.model_name.clone(), "anthropic")
            }
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": format!(
                            "unknown model_config_id '{}' — not found in proxy registries",
                            body.model_config_id
                        ),
                    })),
                );
            }
        }
    } else if !body.model.is_empty() {
        // Legacy path — raw Anthropic model name, default credential
        // implied. Accepted for one release while clients migrate.
        (body.model.clone(), "anthropic")
    } else {
        // Neither field provided — default to Haiku 4.5 for back-compat.
        (default_anthropic_model(), "anthropic")
    };
    let _ = provider_label; // currently always "anthropic"; reserved for future routing.

    let api_key = match state
        .secrets
        .resolve(ProviderId::Anthropic, credential_name.as_str())
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

    // Auto-detect speaker tags in the input. Even when the request flag
    // is false, transcripts that contain `[Name]`-style markers must be
    // formatted with the multi-speaker rules — otherwise the model has
    // no instruction telling it to put speaker transitions on their own
    // paragraph, and on-demand reformat over a stitched-together
    // dialog returns `{"changed": false}` because the prompt didn't ask
    // for paragraph splits at speaker boundaries.
    let multi_speaker = body.multi_speaker || input_has_speaker_tags(&body.text);

    let mut system_prompt = if multi_speaker {
        format!(
            "{}\n\n\
            ADDITIONAL RULES FOR MULTI-SPEAKER DIALOG:\n\
            This transcript is a DIALOG between two or more people, not a monolog.\n\
            Each paragraph may begin with structural markers that you must preserve exactly:\n\
            - Speaker tags: [Name] — identifies who is speaking (e.g. [Gavin], [Dave], [Remote], [Speaker 3]).\n\
            - Timestamps: [MM:SS] or [H:MM:SS] — when the turn was spoken.\n\
            A typical line looks like: [05:23] [Gavin] So I was thinking about the architecture…\n\
            Or without timestamps: [Dave] Yeah, that makes sense.\n\n\
            CRITICAL: Speaker tags appear ONLY at speaker transitions — the first time a different person speaks.\n\
            Consecutive text from the same speaker does NOT repeat the [Name] tag. For example:\n\
            [Gavin] First point about the architecture. And here is a follow-up thought.\n\
            NOT: [Gavin] First point. [Gavin] And here is a follow-up.\n\n\
            Rules:\n\
            1. NEVER remove, rewrite, or reorder speaker tags or timestamps. They are structural, not content. \
            Proper-noun spellings inside speaker tags are also preserved verbatim — even if you suspect a typo.\n\
            2. Every speaker tag MUST begin a new paragraph, preceded by a blank line. This is REQUIRED. \
            If the input contains any speaker tag that is not at the start of a paragraph, you must move it to \
            the start of a new paragraph by inserting a blank line before it. Never merge text from \
            different speakers into the same paragraph.\n\
            3. Consecutive turns from the SAME speaker join into one paragraph WITHOUT repeating the tag or timestamp.\n\
            4. If you add a paragraph break WITHIN a single speaker's text, do NOT add a [Name] tag \
            to the continuation paragraph — the speaker context carries implicitly until a new tag appears.\n\
            5. If you encounter redundant same-speaker tags mid-paragraph (e.g. [Gavin] text [Gavin] more text), \
            remove the duplicate tag so only the first one remains.\n\
            6. Apply paragraph breaks, punctuation, and formatting ONLY within a speaker's text — \
            the words after the [Name] tag.\n\n\
            === EXAMPLE — what \"changed: true\" looks like ===\n\
            INPUT (one paragraph, inline tags, run-on sentences):\n\
            [Gavin] Trey just sat down with the Secretary they talked about a lot of things including the Iran conflict and the dinner he joins us from the State Department what were your top takeaways Trey [Remote] Data, good morning we just spoke with the Secretary for an exclusive interview his first sit-down since the ceasefire we talked not just about that conflict but also the negotiations between Israel and Lebanon\n\n\
            OUTPUT (paragraph per speaker, sentences punctuated):\n\
            [Gavin] Trey just sat down with the Secretary. They talked about a lot of things, including the Iran conflict and the dinner. He joins us from the State Department. What were your top takeaways, Trey?\\n\\n[Remote] Data, good morning. We just spoke with the Secretary for an exclusive interview, his first sit-down since the ceasefire. We talked not just about that conflict, but also the negotiations between Israel and Lebanon.\n\n\
            Notice: every word is preserved in order; the only changes are paragraph breaks at the speaker transition, sentence-ending periods, and a question mark.",
            FORMAT_SYSTEM_PROMPT
        )
    } else {
        FORMAT_SYSTEM_PROMPT.to_string()
    };

    if likely_needs_paragraph_split(&body.text) {
        system_prompt.push_str(
            "\n\nREQUEST-SPECIFIC PARAGRAPH REQUIREMENT:\n\
            The EDITABLE text is a long single paragraph. Returning \
            {\"changed\": false} is invalid for this request. You MUST return \
            {\"changed\": true, \"formatted\": \"...\"} and insert paragraph \
            breaks at the best semantic boundaries you can infer from the \
            content. Preserve every spoken word in order; only change \
            punctuation, capitalization, and whitespace.",
        );
    }

    eprintln!(
        "[proxy] /format dispatching to model={} multi_speaker={} (resolved)",
        model_name, multi_speaker,
    );

    // 8192 (was 4096) — a 3000-char input where the formatter has to
    // re-emit the entire string with paragraph breaks and punctuation
    // can land near the 4096 ceiling once JSON escaping is factored
    // in, and Sonnet sometimes bails to `{"changed": false}` rather
    // than truncate. 8192 is well within Anthropic's per-request
    // budget and gives plenty of headroom for our typical inputs.
    let payload = serde_json::json!({
        "model": model_name,
        "max_tokens": 8192,
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
        .post(&state.anthropic_messages_url)
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
            // Log the raw response (truncated) so we can see exactly
            // what the model sent back when we don't see the change
            // we expect. Length is logged separately because the
            // truncated preview omits the bulk of large outputs.
            let preview: String = full_json.chars().take(240).collect();
            eprintln!(
                "[proxy] /format anthropic response: {}B in, {}B out tokens, {}B body, preview={:?}",
                input_tokens,
                output_tokens,
                full_json.len(),
                preview,
            );
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
                    let changed = parsed["changed"].as_bool().unwrap_or(false);
                    // Server-side validation: reject if the formatter changed
                    // the canonical letter sequence (see `letter_sequence`).
                    if let Some(formatted) = parsed["formatted"].as_str() {
                        let mut final_formatted = formatted.to_string();
                        if let Some(with_breaks) =
                            apply_safe_structural_fallback(&final_formatted, multi_speaker)
                        {
                            eprintln!(
                                "[proxy] formatter output still lacked obvious structural breaks — applying safe fallback"
                            );
                            final_formatted = with_breaks;
                        }
                        if likely_needs_paragraph_split(&final_formatted)
                            && let Some(with_breaks) = insert_readability_breaks(&final_formatted)
                        {
                            eprintln!(
                                "[proxy] formatter output still looked like one long paragraph — applying readability fallback"
                            );
                            final_formatted = with_breaks;
                        }

                        let orig_letters = letter_sequence(&body.text);
                        let fmt_letters = letter_sequence(&final_formatted);
                        if orig_letters != fmt_letters {
                            eprintln!(
                                "[proxy] formatter changed letters — rejecting response \
                                 (orig {} chars, fmt {} chars)",
                                orig_letters.len(),
                                fmt_letters.len(),
                            );
                            if let Some(formatted) =
                                apply_deterministic_formatting_fallback(&body.text, multi_speaker)
                            {
                                eprintln!(
                                    "[proxy] rejected formatter text still needed structure — applying deterministic fallback"
                                );
                                return (
                                    StatusCode::OK,
                                    Json(serde_json::json!({
                                        "changed": true,
                                        "formatted": formatted,
                                        "input_tokens": input_tokens,
                                        "output_tokens": output_tokens,
                                    })),
                                );
                            }
                            return (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "changed": false,
                                    "input_tokens": input_tokens,
                                    "output_tokens": output_tokens,
                                })),
                            );
                        }
                        // Letter sequences match. If word counts differ, the
                        // formatter merged or split tokens at whitespace
                        // boundaries — log a one-line diagnostic so we can
                        // observe how often this is firing in practice.
                        let orig_words = body.text.split_whitespace().count();
                        let fmt_words = final_formatted.split_whitespace().count();
                        if orig_words != fmt_words {
                            eprintln!(
                                "[proxy] formatter merged/split tokens \
                                 (orig {orig_words} words → fmt {fmt_words} words)"
                            );
                        }
                        let final_changed = changed || final_formatted != body.text;
                        let mut result = parsed;
                        result["changed"] = serde_json::json!(final_changed);
                        result["formatted"] = serde_json::json!(final_formatted);
                        result["input_tokens"] = serde_json::json!(input_tokens);
                        result["output_tokens"] = serde_json::json!(output_tokens);
                        return (StatusCode::OK, Json(result));
                    } else if !changed {
                        // The formatter returned `{"changed": false}`.
                        // Surface a one-line diagnostic flagging the
                        // suspicious cases so we can see when the model
                        // is being too conservative — multi-speaker
                        // input that's still in one paragraph almost
                        // always needs at least a paragraph split.
                        let speaker_tags = input_has_speaker_tags(&body.text);
                        let has_paragraph_break = body.text.contains("\n\n");
                        let likely_split_needed = likely_needs_paragraph_split(&body.text);
                        let suspicious = speaker_tags && !has_paragraph_break;
                        eprintln!(
                            "[proxy] formatter declined to change anything \
                             ({} chars, multi_speaker={multi_speaker}, \
                             speaker_tags={speaker_tags}, \
                             has_paragraph_break={has_paragraph_break}, \
                             likely_split_needed={likely_split_needed}{})",
                            body.text.len(),
                            if suspicious {
                                ", SUSPICIOUS: speaker tags but no paragraph break"
                            } else {
                                ""
                            },
                        );
                        if let Some(formatted) =
                            apply_safe_structural_fallback(&body.text, multi_speaker)
                        {
                            eprintln!(
                                "[proxy] formatter declined an obvious structural split — applying safe fallback"
                            );
                            return (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "changed": true,
                                    "formatted": formatted,
                                    "input_tokens": input_tokens,
                                    "output_tokens": output_tokens,
                                })),
                            );
                        }
                        if likely_split_needed
                            && let Some(formatted) = insert_readability_breaks(&body.text)
                        {
                            eprintln!(
                                "[proxy] formatter declined a long single paragraph — applying readability fallback"
                            );
                            return (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "changed": true,
                                    "formatted": formatted,
                                    "input_tokens": input_tokens,
                                    "output_tokens": output_tokens,
                                })),
                            );
                        }
                    } else if let Some(formatted) =
                        apply_deterministic_formatting_fallback(&body.text, multi_speaker)
                    {
                        eprintln!(
                            "[proxy] formatter response omitted formatted text — applying deterministic fallback"
                        );
                        return (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "changed": true,
                                "formatted": formatted,
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
                    eprintln!("[proxy] Haiku returned invalid JSON: {full_json}");
                    if let Some(formatted) =
                        apply_deterministic_formatting_fallback(&body.text, multi_speaker)
                    {
                        eprintln!(
                            "[proxy] invalid formatter JSON — applying deterministic fallback"
                        );
                        return (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "changed": true,
                                "formatted": formatted,
                                "input_tokens": input_tokens,
                                "output_tokens": output_tokens,
                            })),
                        );
                    }
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

/// True when the input contains any `[Word]`-style bracketed marker that
/// looks like a speaker tag or timestamp. Used to auto-activate the
/// multi-speaker prompt rules even when the caller didn't set the
/// `multi_speaker` flag — without this, on-demand reformat over a
/// stitched-together dialog skips the paragraph-split rules and the
/// model returns `{"changed": false}`.
///
/// Detection is deliberately permissive: any `[...]` containing at
/// least one non-whitespace character qualifies. Speaker tags
/// (`[Gavin]`, `[Speaker 3]`), timestamps (`[05:23]`, `[1:02:33]`), and
/// future structural markers all match.
fn input_has_speaker_tags(text: &str) -> bool {
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '[' {
            continue;
        }
        let mut saw_content = false;
        for inner in chars.by_ref() {
            if inner == ']' {
                if saw_content {
                    return true;
                }
                break;
            }
            if !inner.is_whitespace() {
                saw_content = true;
            }
        }
    }
    false
}

/// Insert paragraph breaks only at boundaries that are structurally obvious
/// and preserve the canonical letter sequence. This is a safety net for the
/// formatter's conservative failure mode: if the model returns `changed:false`
/// or emits punctuation without splitting, the proxy can still honor hard
/// speaker transitions and explicit topic-shift phrases without inventing or
/// deleting words.
fn apply_safe_structural_fallback(text: &str, multi_speaker: bool) -> Option<String> {
    let mut formatted = text.to_string();

    if multi_speaker || input_has_speaker_tags(text) {
        formatted = insert_breaks_before_inline_markers(&formatted);
    }

    formatted = insert_breaks_before_topic_shift_cues(&formatted);

    (formatted != text).then_some(formatted)
}

fn apply_deterministic_formatting_fallback(text: &str, multi_speaker: bool) -> Option<String> {
    let mut formatted = text.to_string();

    if let Some(with_breaks) = apply_safe_structural_fallback(&formatted, multi_speaker) {
        formatted = with_breaks;
    }

    if let Some(with_breaks) = insert_readability_breaks(&formatted) {
        formatted = with_breaks;
    }

    (formatted != text).then_some(formatted)
}

fn likely_needs_paragraph_split(text: &str) -> bool {
    paragraph_slices(text)
        .iter()
        .any(|paragraph| paragraph_likely_needs_split(paragraph.text))
}

fn insert_readability_breaks(text: &str) -> Option<String> {
    let paragraphs = paragraph_slices(text);
    if !paragraphs
        .iter()
        .any(|paragraph| paragraph_likely_needs_split(paragraph.text))
    {
        return None;
    }

    let mut formatted = text.to_string();
    let mut changed = false;

    for paragraph in paragraphs.into_iter().rev() {
        if let Some(replacement) = insert_readability_breaks_in_paragraph(paragraph.text) {
            formatted.replace_range(paragraph.start..paragraph.end, &replacement);
            changed = true;
        }
    }

    changed.then_some(formatted)
}

#[derive(Clone, Copy)]
struct ParagraphSlice<'a> {
    start: usize,
    end: usize,
    text: &'a str,
}

fn paragraph_slices(text: &str) -> Vec<ParagraphSlice<'_>> {
    let mut paragraphs = Vec::new();
    let mut start = 0;
    let mut newline_run_start: Option<usize> = None;
    let mut newline_run_count = 0usize;

    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            if newline_run_count == 0 {
                newline_run_start = Some(idx);
            }
            newline_run_count += 1;
            continue;
        }

        if newline_run_count >= 2 {
            let end = newline_run_start.unwrap_or(idx);
            paragraphs.push(ParagraphSlice {
                start,
                end,
                text: &text[start..end],
            });
            start = idx;
        }

        newline_run_start = None;
        newline_run_count = 0;
    }

    if newline_run_count >= 2 {
        let end = newline_run_start.unwrap_or(text.len());
        paragraphs.push(ParagraphSlice {
            start,
            end,
            text: &text[start..end],
        });
        start = text.len();
    }

    paragraphs.push(ParagraphSlice {
        start,
        end: text.len(),
        text: &text[start..],
    });

    paragraphs
}

fn paragraph_likely_needs_split(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    let words = count_words(trimmed);
    let sentence_boundaries = sentence_boundary_indices(trimmed).len();
    trimmed.len() >= 900 || words >= 120 || sentence_boundaries >= 6
}

fn insert_readability_breaks_in_paragraph(text: &str) -> Option<String> {
    if !paragraph_likely_needs_split(text) {
        return None;
    }

    let sentence_boundaries = sentence_boundary_indices(text);
    if sentence_boundaries.len() >= 6 {
        return Some(split_by_sentence_groups(text, &sentence_boundaries, 3));
    }

    if count_words(text) >= 120 {
        return split_by_word_count(text, 90);
    }

    None
}

fn sentence_boundary_indices(text: &str) -> Vec<usize> {
    let mut boundaries = Vec::new();
    let mut iter = text.char_indices().peekable();
    while let Some((idx, ch)) = iter.next() {
        if !matches!(ch, '.' | '?' | '!') {
            continue;
        }
        let end = idx + ch.len_utf8();
        let next = iter.peek().map(|(_, next)| *next);
        if next.map(|c| c.is_whitespace()).unwrap_or(true) {
            boundaries.push(end);
        }
    }
    boundaries
}

fn split_by_sentence_groups(
    text: &str,
    boundaries: &[usize],
    sentences_per_paragraph: usize,
) -> String {
    let mut out = String::with_capacity(text.len() + boundaries.len());
    let mut last = 0;
    for (idx, boundary) in boundaries.iter().enumerate() {
        out.push_str(&text[last..*boundary]);
        last = *boundary;
        if (idx + 1) % sentences_per_paragraph == 0 && idx + 1 < boundaries.len() {
            out.push_str("\n\n");
            last = skip_leading_whitespace(text, last);
        }
    }
    out.push_str(&text[last..]);
    out
}

fn split_by_word_count(text: &str, words_per_paragraph: usize) -> Option<String> {
    let mut out = String::with_capacity(text.len() + text.len() / words_per_paragraph.max(1));
    let mut word_count = 0usize;
    let mut inserted_break = false;
    let mut previous_end = 0usize;

    for (start, word) in word_spans(text) {
        out.push_str(&text[previous_end..start]);
        if word_count > 0 && word_count % words_per_paragraph == 0 {
            let trimmed_len = out.trim_end_matches(|c: char| c.is_whitespace()).len();
            out.truncate(trimmed_len);
            out.push_str("\n\n");
            inserted_break = true;
        }
        out.push_str(word);
        previous_end = start + word.len();
        word_count += 1;
    }
    out.push_str(&text[previous_end..]);

    inserted_break.then_some(out)
}

fn word_spans(text: &str) -> Vec<(usize, &str)> {
    let mut spans = Vec::new();
    let mut in_word = false;
    let mut word_start = 0usize;
    for (idx, ch) in text.char_indices() {
        if ch.is_whitespace() {
            if in_word {
                spans.push((word_start, &text[word_start..idx]));
                in_word = false;
            }
        } else if !in_word {
            in_word = true;
            word_start = idx;
        }
    }
    if in_word {
        spans.push((word_start, &text[word_start..]));
    }
    spans
}

fn skip_leading_whitespace(text: &str, mut idx: usize) -> usize {
    while idx < text.len() {
        let Some(ch) = text[idx..].chars().next() else {
            break;
        };
        if !ch.is_whitespace() {
            break;
        }
        idx += ch.len_utf8();
    }
    idx
}

fn insert_breaks_before_inline_markers(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut paragraph_prefix = String::new();
    let mut chars = text.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        if ch == '[' && bracketed_marker_end(text, idx).is_some() {
            let previous_nonspace = paragraph_prefix.chars().any(|c| !c.is_whitespace());
            if previous_nonspace && !prefix_is_only_markers_and_space(&paragraph_prefix) {
                let trimmed_len = out.trim_end_matches(|c: char| c.is_whitespace()).len();
                out.truncate(trimmed_len);
                if !out.ends_with("\n\n") {
                    out.push_str("\n\n");
                }
                paragraph_prefix.clear();
            }
        }

        out.push(ch);
        paragraph_prefix.push(ch);
        if ch == '\n' && text[idx..].starts_with("\n\n") {
            paragraph_prefix.clear();
        }
    }

    out
}

fn bracketed_marker_end(text: &str, start: usize) -> Option<usize> {
    let mut saw_content = false;
    for (offset, ch) in text[start + 1..].char_indices() {
        if ch == ']' {
            return saw_content.then_some(start + 1 + offset);
        }
        if !ch.is_whitespace() {
            saw_content = true;
        }
    }
    None
}

fn prefix_is_only_markers_and_space(prefix: &str) -> bool {
    let mut idx = 0;
    while idx < prefix.len() {
        let rest = &prefix[idx..];
        let Some(ch) = rest.chars().next() else {
            break;
        };
        if ch.is_whitespace() {
            idx += ch.len_utf8();
            continue;
        }
        if ch == '['
            && let Some(end) = bracketed_marker_end(prefix, idx)
        {
            idx = end + 1;
            continue;
        }
        return false;
    }
    true
}

fn insert_breaks_before_topic_shift_cues(text: &str) -> String {
    const CUES: &[&str] = &[
        "switching topics",
        "switching gears",
        "on a different note",
        "moving on",
        "another thing",
        "the next thing",
        "next topic",
        "now let's talk about",
        "now lets talk about",
        "i also want to talk about",
        "i want to talk about",
    ];

    let lower = text.to_lowercase();
    let mut positions: Vec<usize> = CUES
        .iter()
        .flat_map(|cue| find_word_boundary_matches(&lower, cue))
        .filter(|pos| {
            let before = &text[..*pos];
            let after = &text[*pos..];
            !before.ends_with("\n\n")
                && count_words(before) >= 20
                && count_words(after) >= 8
                && !same_paragraph_prefix_ends_with_break(before)
        })
        .collect();
    positions.sort_unstable();
    positions.dedup();

    if positions.is_empty() {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len() + positions.len() * 2);
    let mut last = 0;
    for pos in positions {
        out.push_str(&text[last..pos]);
        let trimmed_len = out.trim_end_matches(|c: char| c.is_whitespace()).len();
        out.truncate(trimmed_len);
        if !out.ends_with("\n\n") {
            out.push_str("\n\n");
        }
        last = pos;
    }
    out.push_str(&text[last..]);
    out
}

fn find_word_boundary_matches(haystack_lower: &str, needle_lower: &str) -> Vec<usize> {
    let mut matches = Vec::new();
    let mut start = 0;
    while let Some(relative) = haystack_lower[start..].find(needle_lower) {
        let pos = start + relative;
        let end = pos + needle_lower.len();
        if is_word_boundary(haystack_lower, pos) && is_word_boundary(haystack_lower, end) {
            matches.push(pos);
        }
        start = end;
    }
    matches
}

fn is_word_boundary(text: &str, idx: usize) -> bool {
    if idx == 0 || idx == text.len() {
        return true;
    }
    let before = text[..idx].chars().next_back();
    let after = text[idx..].chars().next();
    !matches!((before, after), (Some(a), Some(b)) if a.is_alphanumeric() && b.is_alphanumeric())
}

fn count_words(text: &str) -> usize {
    text.split_whitespace().count()
}

fn same_paragraph_prefix_ends_with_break(text_before: &str) -> bool {
    text_before
        .rsplit_once("\n\n")
        .map(|(_, tail)| tail.trim().is_empty())
        .unwrap_or(false)
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
///
/// Retained for unit-test coverage of the original word-list normalization
/// even though [`words_match`] now uses the looser letter-sequence
/// comparison via [`letter_sequence`].
#[cfg(test)]
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
/// them still fails the comparison.
///
/// Retained for unit-test coverage of the original word-list matcher even
/// though [`words_match`] now uses the looser letter-sequence comparison.
#[cfg(test)]
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

/// Reduce a transcript to a normalized letter sequence: strip bracketed
/// structural markers (speaker tags, timestamps), drop every non-alphanumeric
/// character (whitespace, punctuation, apostrophes, hyphens, em-dashes,
/// ellipses), and lowercase the result.
///
/// This is the canonical form the formatter must preserve. Allowed changes
/// are anything that does not alter the letter/digit sequence:
///
/// - Punctuation insertion/removal (commas, periods, question marks, etc.)
/// - Capitalization changes
/// - Whitespace re-arrangement: paragraph breaks, line wrapping, **and
///   merging or splitting tokens** at word boundaries (the STT can deliver
///   a single word as `"speci es"`; the formatter is allowed to glue it
///   back to `"species"` because the letter sequence is unchanged)
/// - Apostrophe / hyphen / em-dash / ellipsis insertion or removal
/// - Acronym / identifier joining (`"U S A"` → `"USA"`, `"G P T 4"` → `"GPT4"`)
/// - Dropping redundant bracketed markers (`[Gavin] x [Gavin] y` → `[Gavin] x y`)
///
/// Forbidden (these change the letter sequence and are rejected):
///
/// - Adding, removing, or substituting any letter or digit
/// - Reordering letters or digits
/// - Spelling out numbers (`"5"` ↔ `"five"`) — the digits/letters differ
/// - Diacritic changes (`"naïve"` vs `"naive"`) — `i` vs `ï` differ
fn letter_sequence(text: &str) -> String {
    strip_bracketed_markers(text)
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Compare two texts as canonical letter sequences. See [`letter_sequence`]
/// for the precise rules.
///
/// Production callsites inline `letter_sequence(...) == letter_sequence(...)`
/// directly so they can also surface the diagnostic counts; this helper is
/// retained for the test suite where the boolean answer is the only thing
/// the assertions care about.
#[cfg(test)]
fn words_match(original: &str, formatted: &str) -> bool {
    letter_sequence(original) == letter_sequence(formatted)
}

#[tokio::main]
async fn main() {
    let cors = CorsLayer::very_permissive();

    let client = reqwest::Client::builder()
        // No total `.timeout()` — this client is shared with the
        // streaming LLM and TTS providers, where a single response
        // body can legitimately take minutes (Opus replies +
        // sentence-by-sentence ElevenLabs synthesis). A global
        // timeout would chop those streams mid-flight and surface
        // as "transport error: error decoding response body".
        // Per-request deadlines for short calls (token mint,
        // formatting) are the right pattern if we ever need them.
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
        soniox_temporary_api_key_url: SONIOX_TEMPORARY_API_KEY_URL.to_string(),
        anthropic_messages_url: ANTHROPIC_MESSAGES_URL.to_string(),
        registries: registries.clone(),
    });
    let conversation_state = conversation_api::ConversationApiState::new(
        registries,
        client.clone(),
        secrets_manager.clone(),
    );
    let secrets_state = secrets_api::SecretsApiState::new(secrets_manager.clone());
    let stt_api_state = stt_api::SttApiState::new(client.clone(), secrets_manager.clone());
    let tts_api_state = tts_api::TtsApiState::new(client, secrets_manager);

    let app = Router::new()
        .route("/token", post(fetch_token))
        .route("/api/stt/soniox/token", post(fetch_soniox_token))
        .route("/format", post(format_text))
        .with_state(state)
        .merge(conversation_api::router(conversation_state))
        .merge(secrets_api::router(secrets_state))
        .merge(stt_api::router(stt_api_state))
        .merge(tts_api::router(tts_api_state))
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
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use secrets::{InMemoryKeyStore, StaticEnv};
    use tower::ServiceExt;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_secrets(soniox_key: Option<&str>) -> Arc<SecretsManager> {
        let mut env = StaticEnv::new();
        if let Some(key) = soniox_key {
            env.set("PARLEY_SONIOX_API_KEY", key);
        }
        let temp = tempfile::tempdir().expect("tempdir");
        Arc::new(SecretsManager::new(
            Box::new(InMemoryKeyStore::new()),
            Box::new(env),
            temp.path().join("credentials.json"),
        ))
    }

    fn empty_registries() -> Arc<conversation_api::Registries> {
        Arc::new(conversation_api::Registries {
            personas: std::collections::HashMap::new(),
            models: std::collections::HashMap::new(),
            prompts_dir: PathBuf::from("."),
            sessions_dir: PathBuf::from("."),
        })
    }

    fn soniox_token_test_app(upstream_url: String, secrets: Arc<SecretsManager>) -> Router {
        let state = Arc::new(AppState {
            client: reqwest::Client::new(),
            secrets,
            soniox_temporary_api_key_url: upstream_url,
            anthropic_messages_url: ANTHROPIC_MESSAGES_URL.to_string(),
            registries: empty_registries(),
        });
        Router::new()
            .route("/api/stt/soniox/token", post(fetch_soniox_token))
            .with_state(state)
    }

    async fn response_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        serde_json::from_slice(&bytes).expect("json body")
    }

    // ── Soniox token route ─────────────────────────────────────────

    #[tokio::test]
    async fn soniox_token_missing_credential_returns_412() {
        let app = soniox_token_test_app("http://127.0.0.1/unused".to_string(), test_secrets(None));

        let response = app
            .oneshot(
                Request::post("/api/stt/soniox/token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
        let body = response_json(response).await;
        assert_eq!(body["error"], "provider_not_configured");
        assert_eq!(body["provider"], "soniox");
        assert_eq!(body["credential"], "default");
    }

    #[tokio::test]
    async fn soniox_token_success_returns_temporary_key_only() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/auth/temporary-api-key"))
            .and(header("authorization", "Bearer root-soniox-key"))
            .and(body_json(serde_json::json!({
                "usage_type": "transcribe_websocket",
                "expires_in_seconds": 480,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "api_key": "temporary-soniox-key",
                "expires_at": "2026-01-01T00:00:00Z",
                "ignored": "not forwarded",
            })))
            .mount(&server)
            .await;
        let app = soniox_token_test_app(
            format!("{}/v1/auth/temporary-api-key", server.uri()),
            test_secrets(Some("root-soniox-key")),
        );

        let response = app
            .oneshot(
                Request::post("/api/stt/soniox/token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["api_key"], "temporary-soniox-key");
        assert_eq!(body["expires_at"], "2026-01-01T00:00:00Z");
        assert!(body.get("ignored").is_none());
    }

    #[tokio::test]
    async fn soniox_token_upstream_error_does_not_leak_secret() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/auth/temporary-api-key"))
            .respond_with(ResponseTemplate::new(401).set_body_string("invalid root key"))
            .mount(&server)
            .await;
        let app = soniox_token_test_app(
            format!("{}/v1/auth/temporary-api-key", server.uri()),
            test_secrets(Some("root-soniox-key")),
        );

        let response = app
            .oneshot(
                Request::post("/api/stt/soniox/token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = response_json(response).await;
        let error = body["error"].as_str().unwrap();
        assert!(error.contains("Soniox HTTP 401 Unauthorized"));
        assert!(!error.contains("root-soniox-key"));
    }

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
        // the letter sequences differ ("usabcontext" vs "usacontext") and
        // the validator must reject.
        assert!(!words_match("u s a b context", "USA context"));
    }

    // ── words_match: token merging / splitting (Soniox subword fix) ─

    #[test]
    fn words_match_allows_subword_merge() {
        // Soniox sometimes splits a single word across two tokens with a
        // stray space ("speci es"). The formatter is allowed to glue the
        // pieces back together — letter sequence is unchanged.
        assert!(words_match(
            "let's talk about cheetahs and origin of speci es",
            "Let's talk about cheetahs and origin of species.",
        ));
        assert!(words_match(
            "Encyclop edia of Cheetahs",
            "Encyclopedia of Cheetahs",
        ));
        assert!(words_match("Somet imes", "Sometimes"));
    }

    #[test]
    fn words_match_allows_word_split() {
        // The inverse direction also passes (the formatter shouldn't
        // normally do this, but if it splits "homepage" → "home page" the
        // letter sequence is preserved and we don't reject).
        assert!(words_match("homepage", "home page"));
    }

    #[test]
    fn words_match_rejects_different_letters_even_if_close_to_a_merge() {
        // "speci es" → "specie" drops a letter; reject.
        assert!(!words_match("speci es", "specie"));
        // "speci es" → "speci ess" adds a letter; reject.
        assert!(!words_match("speci es", "speci ess"));
    }

    // ── words_match: punctuation-only changes the prompt allows ─────

    #[test]
    fn words_match_allows_apostrophe_addition() {
        // STT often drops apostrophes in contractions; the formatter is
        // allowed to put them back. Apostrophe is dropped from the
        // letter sequence on both sides.
        assert!(words_match("dont stop believin", "Don't stop believin'."));
    }

    #[test]
    fn words_match_allows_hyphen_addition_or_removal() {
        assert!(words_match("well known author", "well-known author"));
        assert!(words_match("state of the art", "state-of-the-art"));
    }

    #[test]
    fn words_match_rejects_number_word_substitution() {
        // Spelling out a number changes the digit/letter sequence even
        // though the meaning is the same. Reject — the formatter is not
        // a transformation engine.
        assert!(!words_match("we have 5 apples", "we have five apples"));
        assert!(!words_match("we have five apples", "we have 5 apples"));
    }

    #[test]
    fn words_match_rejects_diacritic_change() {
        // "naïve" vs "naive" differ in character (`ï` vs `i`) — reject.
        assert!(!words_match("the naïve user", "the naive user"));
    }

    // ── input_has_speaker_tags (auto-detect multi-speaker mode) ────

    #[test]
    fn input_has_speaker_tags_finds_named_tag() {
        assert!(input_has_speaker_tags("[Gavin] hello world"));
        assert!(input_has_speaker_tags(
            "And he said hi. [Remote] Yes indeed."
        ));
        assert!(input_has_speaker_tags("[Speaker 3] you were there"));
    }

    #[test]
    fn input_has_speaker_tags_finds_timestamp() {
        assert!(input_has_speaker_tags("[05:23] hello there"));
        assert!(input_has_speaker_tags("[1:02:33] [Gavin] yo"));
    }

    #[test]
    fn input_has_speaker_tags_ignores_empty_brackets() {
        // Empty brackets shouldn't trip detection — they aren't
        // speaker tags and likely came from the user's prose.
        assert!(!input_has_speaker_tags("hello [] world"));
        assert!(!input_has_speaker_tags("see footnote [   ] here"));
    }

    #[test]
    fn input_has_speaker_tags_returns_false_when_no_brackets() {
        assert!(!input_has_speaker_tags("just plain text without tags"));
        assert!(!input_has_speaker_tags(""));
    }

    #[test]
    fn input_has_speaker_tags_handles_unmatched_open_bracket() {
        // A stray `[` with no closing `]` shouldn't false-positive.
        assert!(!input_has_speaker_tags("see [open with no close"));
    }

    #[test]
    fn input_has_speaker_tags_real_news_transcript_excerpt() {
        // Regression: the 4-speaker excerpt from the field report had
        // `[Gavin]`, `[Remote]`, `[Speaker 3]`, `[Speaker 4]` inline
        // and the on-demand reformat returned `{"changed": false}`
        // because the request flag was off. Auto-detect must catch
        // this case so the multi-speaker prompt branch fires.
        let text = "[Gavin] And just moments ago, Trey sat down with the Secretary of State \
                    Marco Rubio in an exclusive interview. [Remote] Data, good morning.";
        assert!(input_has_speaker_tags(text));
    }

    // ── safe structural fallback ──────────────────────────────────

    #[test]
    fn structural_fallback_splits_inline_speaker_transition() {
        let text =
            "[Gavin] First point about the interview [Remote] Second point from the remote anchor";
        let formatted = apply_safe_structural_fallback(text, false)
            .expect("inline speaker transition should split");
        assert_eq!(
            formatted,
            "[Gavin] First point about the interview\n\n[Remote] Second point from the remote anchor"
        );
        assert!(words_match(text, &formatted));
    }

    #[test]
    fn structural_fallback_keeps_timestamp_and_speaker_prefix_together() {
        let text = "[05:23] [Gavin] First point about the interview [Remote] Second point";
        let formatted = apply_safe_structural_fallback(text, false)
            .expect("inline second speaker should split");
        assert_eq!(
            formatted,
            "[05:23] [Gavin] First point about the interview\n\n[Remote] Second point"
        );
        assert!(!formatted.contains("[05:23]\n\n[Gavin]"));
        assert!(words_match(text, &formatted));
    }

    #[test]
    fn structural_fallback_splits_explicit_topic_shift_cue() {
        let text = "We need to talk through the budget because the last estimate missed hosting storage logging and support costs for the pilot rollout switching topics the launch plan also needs a dry run with support product and engineering before we announce anything publicly";
        let formatted =
            apply_safe_structural_fallback(text, false).expect("explicit topic shift should split");
        assert_eq!(
            formatted,
            "We need to talk through the budget because the last estimate missed hosting storage logging and support costs for the pilot rollout\n\nswitching topics the launch plan also needs a dry run with support product and engineering before we announce anything publicly"
        );
        assert!(words_match(text, &formatted));
    }

    #[test]
    fn long_single_paragraph_is_considered_split_needed() {
        let text = "First sentence sets up the context for the discussion. Second sentence adds more detail about the current state. Third sentence explains the first concern. Fourth sentence moves into a related operational concern. Fifth sentence starts to summarize the risk. Sixth sentence introduces the next decision that needs attention.";
        assert!(likely_needs_paragraph_split(text));
    }

    #[test]
    fn readability_fallback_splits_long_sentence_sequence() {
        let text = "First sentence sets up the context for the discussion. Second sentence adds more detail about the current state. Third sentence explains the first concern. Fourth sentence moves into a related operational concern. Fifth sentence starts to summarize the risk. Sixth sentence introduces the next decision that needs attention.";
        let formatted = insert_readability_breaks(text)
            .expect("six sentence paragraph should receive a readability split");
        assert_eq!(
            formatted,
            "First sentence sets up the context for the discussion. Second sentence adds more detail about the current state. Third sentence explains the first concern.\n\nFourth sentence moves into a related operational concern. Fifth sentence starts to summarize the risk. Sixth sentence introduces the next decision that needs attention."
        );
        assert!(words_match(text, &formatted));
    }

    #[test]
    fn readability_fallback_splits_long_paragraph_even_with_existing_blank_line() {
        let text = "Short setup paragraph.\n\nFirst sentence sets up the context for the discussion. Second sentence adds more detail about the current state. Third sentence explains the first concern. Fourth sentence moves into a related operational concern. Fifth sentence starts to summarize the risk. Sixth sentence introduces the next decision that needs attention.";
        assert!(likely_needs_paragraph_split(text));

        let formatted = insert_readability_breaks(text)
            .expect("long second paragraph should split despite existing blank line");
        assert!(formatted.starts_with("Short setup paragraph.\n\nFirst sentence"));
        assert!(formatted.contains("concern.\n\nFourth sentence"));
        assert!(words_match(text, &formatted));
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

    // ── /format request shape + dispatch ───────────────────────────

    #[test]
    fn format_request_decodes_new_shape() {
        let body = serde_json::json!({
            "context": "old para.",
            "text": "hello world",
            "multi_speaker": true,
            "model_config_id": "haiku",
            "credential": "work",
        });
        let parsed: FormatRequest = serde_json::from_value(body).expect("new shape should decode");
        assert_eq!(parsed.context, "old para.");
        assert_eq!(parsed.text, "hello world");
        assert!(parsed.multi_speaker);
        assert_eq!(parsed.model_config_id, "haiku");
        assert_eq!(parsed.credential, "work");
        assert_eq!(parsed.model, "");
    }

    #[test]
    fn format_request_decodes_legacy_model_alias() {
        // Old clients still send `model` only — must keep working
        // for one release.
        let body = serde_json::json!({
            "text": "hi",
            "model": "claude-haiku-4-5-20251001",
        });
        let parsed: FormatRequest = serde_json::from_value(body).expect("legacy decode");
        assert_eq!(parsed.model, "claude-haiku-4-5-20251001");
        assert_eq!(parsed.model_config_id, "");
    }

    fn format_test_app(
        upstream_url: String,
        secrets: Arc<SecretsManager>,
        registries: Arc<conversation_api::Registries>,
    ) -> Router {
        let state = Arc::new(AppState {
            client: reqwest::Client::new(),
            secrets,
            soniox_temporary_api_key_url: SONIOX_TEMPORARY_API_KEY_URL.to_string(),
            anthropic_messages_url: upstream_url,
            registries,
        });
        Router::new()
            .route("/format", post(format_text))
            .with_state(state)
    }

    fn registries_with_anthropic_haiku() -> Arc<conversation_api::Registries> {
        use parley_core::model_config::{LlmProviderTag, ModelConfig, TokenRates};
        use parley_core::tts::chunking::ChunkPolicy;
        let mut models = std::collections::HashMap::new();
        models.insert(
            "haiku-cfg".to_string(),
            ModelConfig {
                id: "haiku-cfg".to_string(),
                provider: LlmProviderTag::Anthropic,
                model_name: "claude-haiku-4-5-20251001".to_string(),
                context_window: 200_000,
                rates: TokenRates::default(),
                options: serde_json::Value::Null,
                tts_chunking: ChunkPolicy::default(),
            },
        );
        models.insert(
            "openai-cfg".to_string(),
            ModelConfig {
                id: "openai-cfg".to_string(),
                provider: LlmProviderTag::Openai,
                model_name: "gpt-5".to_string(),
                context_window: 200_000,
                rates: TokenRates::default(),
                options: serde_json::Value::Null,
                tts_chunking: ChunkPolicy::default(),
            },
        );
        Arc::new(conversation_api::Registries {
            personas: std::collections::HashMap::new(),
            models,
            prompts_dir: PathBuf::from("."),
            sessions_dir: PathBuf::from("."),
        })
    }

    fn anthropic_secrets() -> Arc<SecretsManager> {
        let mut env = StaticEnv::new();
        env.set("PARLEY_ANTHROPIC_API_KEY", "test-anthropic-key");
        let temp = tempfile::tempdir().expect("tempdir");
        Arc::new(SecretsManager::new(
            Box::new(InMemoryKeyStore::new()),
            Box::new(env),
            temp.path().join("credentials.json"),
        ))
    }

    #[tokio::test]
    async fn format_unknown_model_config_id_returns_400() {
        let app = format_test_app(
            "http://127.0.0.1/unused".to_string(),
            anthropic_secrets(),
            registries_with_anthropic_haiku(),
        );
        let body = serde_json::json!({
            "text": "hello",
            "model_config_id": "no-such-id",
        })
        .to_string();
        let response = app
            .oneshot(
                Request::post("/format")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = response_json(response).await;
        let err = json["error"].as_str().unwrap();
        assert!(err.contains("unknown model_config_id"));
        assert!(err.contains("no-such-id"));
    }

    #[tokio::test]
    async fn format_non_anthropic_model_returns_501() {
        let app = format_test_app(
            "http://127.0.0.1/unused".to_string(),
            anthropic_secrets(),
            registries_with_anthropic_haiku(),
        );
        let body = serde_json::json!({
            "text": "hello",
            "model_config_id": "openai-cfg",
        })
        .to_string();
        let response = app
            .oneshot(
                Request::post("/format")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let json = response_json(response).await;
        let err = json["error"].as_str().unwrap();
        assert!(err.contains("openai"));
        assert!(err.contains("not yet supported"));
    }

    #[tokio::test]
    async fn format_changed_false_with_inline_speaker_transition_gets_safe_fallback() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("x-api-key", "test-anthropic-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{
                    "text": "\"changed\": false}",
                }],
                "usage": { "input_tokens": 22, "output_tokens": 2 },
            })))
            .mount(&server)
            .await;

        let app = format_test_app(
            server.uri(),
            anthropic_secrets(),
            registries_with_anthropic_haiku(),
        );
        let original =
            "[Gavin] First point about the interview [Remote] Second point from the remote anchor";
        let body = serde_json::json!({
            "text": original,
            "model_config_id": "haiku-cfg",
        })
        .to_string();
        let response = app
            .oneshot(
                Request::post("/format")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["changed"], true);
        assert_eq!(
            json["formatted"].as_str().unwrap(),
            "[Gavin] First point about the interview\n\n[Remote] Second point from the remote anchor"
        );
        assert_eq!(json["input_tokens"], 22);
        assert_eq!(json["output_tokens"], 2);
    }

    #[tokio::test]
    async fn format_changed_false_with_long_single_paragraph_gets_readability_fallback() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("x-api-key", "test-anthropic-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{
                    "text": "\"changed\": false}",
                }],
                "usage": { "input_tokens": 77, "output_tokens": 2 },
            })))
            .mount(&server)
            .await;

        let app = format_test_app(
            server.uri(),
            anthropic_secrets(),
            registries_with_anthropic_haiku(),
        );
        let original = "First sentence sets up the context for the discussion. Second sentence adds more detail about the current state. Third sentence explains the first concern. Fourth sentence moves into a related operational concern. Fifth sentence starts to summarize the risk. Sixth sentence introduces the next decision that needs attention.";
        let body = serde_json::json!({
            "text": original,
            "model_config_id": "haiku-cfg",
        })
        .to_string();
        let response = app
            .oneshot(
                Request::post("/format")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["changed"], true);
        let formatted = json["formatted"].as_str().unwrap();
        assert!(formatted.contains("\n\nFourth sentence"));
        assert!(words_match(original, formatted));
        assert_eq!(json["input_tokens"], 77);
        assert_eq!(json["output_tokens"], 2);
    }

    #[tokio::test]
    async fn format_changed_false_with_formatted_field_keeps_fallback_changed_true() {
        let server = MockServer::start().await;
        let original = "First sentence sets up the context for the discussion. Second sentence adds more detail about the current state. Third sentence explains the first concern. Fourth sentence moves into a related operational concern. Fifth sentence starts to summarize the risk. Sixth sentence introduces the next decision that needs attention.";
        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("x-api-key", "test-anthropic-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{
                    "text": format!("\"changed\": false, \"formatted\": {} }}", serde_json::to_string(original).unwrap()),
                }],
                "usage": { "input_tokens": 81, "output_tokens": 64 },
            })))
            .mount(&server)
            .await;

        let app = format_test_app(
            server.uri(),
            anthropic_secrets(),
            registries_with_anthropic_haiku(),
        );
        let body = serde_json::json!({
            "text": original,
            "model_config_id": "haiku-cfg",
        })
        .to_string();
        let response = app
            .oneshot(
                Request::post("/format")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["changed"], true);
        let formatted = json["formatted"].as_str().unwrap();
        assert!(formatted.contains("\n\nFourth sentence"));
        assert!(words_match(original, formatted));
        assert_eq!(json["input_tokens"], 81);
        assert_eq!(json["output_tokens"], 64);
    }

    #[tokio::test]
    async fn format_invalid_json_with_long_single_paragraph_gets_readability_fallback() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("x-api-key", "test-anthropic-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{
                    "text": "\"changed\": true, \"formatted\": \"unterminated",
                }],
                "usage": { "input_tokens": 91, "output_tokens": 725 },
            })))
            .mount(&server)
            .await;

        let app = format_test_app(
            server.uri(),
            anthropic_secrets(),
            registries_with_anthropic_haiku(),
        );
        let original = "First sentence sets up the context for the discussion. Second sentence adds more detail about the current state. Third sentence explains the first concern. Fourth sentence moves into a related operational concern. Fifth sentence starts to summarize the risk. Sixth sentence introduces the next decision that needs attention.";
        let body = serde_json::json!({
            "text": original,
            "model_config_id": "haiku-cfg",
        })
        .to_string();
        let response = app
            .oneshot(
                Request::post("/format")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["changed"], true);
        let formatted = json["formatted"].as_str().unwrap();
        assert!(formatted.contains("\n\nFourth sentence"));
        assert!(words_match(original, formatted));
        assert_eq!(json["input_tokens"], 91);
        assert_eq!(json["output_tokens"], 725);
    }

    #[tokio::test]
    async fn format_rejected_letter_change_uses_deterministic_fallback() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("x-api-key", "test-anthropic-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{
                    "text": "\"changed\": true, \"formatted\": \"Different words entirely.\"}",
                }],
                "usage": { "input_tokens": 93, "output_tokens": 44 },
            })))
            .mount(&server)
            .await;

        let app = format_test_app(
            server.uri(),
            anthropic_secrets(),
            registries_with_anthropic_haiku(),
        );
        let original = "First sentence sets up the context for the discussion. Second sentence adds more detail about the current state. Third sentence explains the first concern. Fourth sentence moves into a related operational concern. Fifth sentence starts to summarize the risk. Sixth sentence introduces the next decision that needs attention.";
        let body = serde_json::json!({
            "text": original,
            "model_config_id": "haiku-cfg",
        })
        .to_string();
        let response = app
            .oneshot(
                Request::post("/format")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["changed"], true);
        let formatted = json["formatted"].as_str().unwrap();
        assert!(formatted.contains("\n\nFourth sentence"));
        assert!(words_match(original, formatted));
        assert_eq!(json["input_tokens"], 93);
        assert_eq!(json["output_tokens"], 44);
    }

    #[tokio::test]
    async fn format_changed_true_without_required_split_gets_safe_fallback() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("x-api-key", "test-anthropic-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{
                    "text": "\"changed\": true, \"formatted\": \"[Gavin] First point about the interview. [Remote] Second point from the remote anchor.\"}",
                }],
                "usage": { "input_tokens": 24, "output_tokens": 18 },
            })))
            .mount(&server)
            .await;

        let app = format_test_app(
            server.uri(),
            anthropic_secrets(),
            registries_with_anthropic_haiku(),
        );
        let original =
            "[Gavin] First point about the interview [Remote] Second point from the remote anchor";
        let body = serde_json::json!({
            "text": original,
            "model_config_id": "haiku-cfg",
        })
        .to_string();
        let response = app
            .oneshot(
                Request::post("/format")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["changed"], true);
        assert_eq!(
            json["formatted"].as_str().unwrap(),
            "[Gavin] First point about the interview.\n\n[Remote] Second point from the remote anchor."
        );
    }

    #[tokio::test]
    async fn format_anthropic_model_config_id_calls_upstream_when_configured() {
        // The Anthropic URL is injected into AppState in tests so the
        // formatter path can be exercised without a live provider key.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/")) // wiremock root path
            .and(header("x-api-key", "test-anthropic-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{
                    "text": "\"changed\": false}",
                }],
                "usage": { "input_tokens": 10, "output_tokens": 2 },
            })))
            .mount(&server)
            .await;

        let app = format_test_app(
            server.uri(),
            anthropic_secrets(),
            registries_with_anthropic_haiku(),
        );
        let body = serde_json::json!({
            "text": "hello",
            "model_config_id": "haiku-cfg",
        })
        .to_string();
        let response = app
            .oneshot(
                Request::post("/format")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["changed"], false);
        assert_eq!(json["input_tokens"], 10);
        assert_eq!(json["output_tokens"], 2);
    }
}
