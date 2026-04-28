//! Settings drawer extracted out of [`crate::ui::app::App`] so it can
//! render at the [`crate::ui::root::Root`] shell level. That makes
//! the gear button useful from both the Transcribe and Conversation
//! views.
//!
//! All state is read from the [`crate::ui::app_state::AppSettings`]
//! context provided in `Root`. The component renders nothing when
//! `show_settings` is false, so it's safe to mount unconditionally.

use dioxus::prelude::*;
use serde::Deserialize;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use crate::stt::soniox::{SONIOX_CONTEXT_TEXT_STORAGE_KEY, SONIOX_LATENCY_MODE_COOKIE};
use crate::ui::app::{SecretsKeyRow, save, save_local, soniox_latency_mode_from_value};
use crate::ui::app_state::AppSettings;

const PROXY_BASE: &str = "http://127.0.0.1:3033";

/// Minimal mirror of the proxy's `ModelSummary`. Kept private to the
/// drawer because the conversation view declares its own copy with
/// the same shape — the WASM root has no shared HTTP-types crate.
#[derive(Debug, Clone, Deserialize)]
struct DrawerModelSummary {
    id: String,
    provider: String,
    #[serde(default)]
    model_name: String,
}

#[derive(Debug, Deserialize)]
struct DrawerModelListResponse {
    models: Vec<DrawerModelSummary>,
}

async fn fetch_models() -> Result<Vec<DrawerModelSummary>, String> {
    let opts = web_sys::RequestInit::new();
    opts.set_method("GET");
    opts.set_mode(web_sys::RequestMode::Cors);
    let req = web_sys::Request::new_with_str_and_init(&format!("{PROXY_BASE}/models"), &opts)
        .map_err(|e| format!("{e:?}"))?;
    let window = web_sys::window().ok_or_else(|| "no window".to_string())?;
    let resp_val = JsFuture::from(window.fetch_with_request(&req))
        .await
        .map_err(|e| format!("{e:?}"))?;
    let resp: web_sys::Response = resp_val
        .dyn_into()
        .map_err(|_| "bad response".to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let json_promise = resp.json().map_err(|e| format!("{e:?}"))?;
    let json_val = JsFuture::from(json_promise)
        .await
        .map_err(|e| format!("{e:?}"))?;
    let s = js_sys::JSON::stringify(&json_val)
        .map_err(|e| format!("{e:?}"))?
        .as_string()
        .ok_or_else(|| "no json string".to_string())?;
    let parsed: DrawerModelListResponse = serde_json::from_str(&s).map_err(|e| e.to_string())?;
    Ok(parsed.models)
}

#[component]
pub fn SettingsDrawer() -> Element {
    let settings: AppSettings = use_context();
    let mut show_settings = settings.show_settings;
    let mut secrets_refresh = settings.secrets_refresh;

    let assemblyai_configured = settings.assemblyai_configured;
    let soniox_configured = settings.soniox_configured;
    let anthropic_configured = settings.anthropic_configured;
    let elevenlabs_configured = settings.elevenlabs_configured;
    let xai_configured = settings.xai_configured;
    let cartesia_configured = settings.cartesia_configured;

    let mut pipeline_tts_provider = settings.pipeline_tts_provider;
    let mut pipeline_tts_voice = settings.pipeline_tts_voice;
    let voice_credential = settings.voice_credential;
    let pipeline_stt_provider = settings.pipeline_stt_provider;
    let mut soniox_latency_mode = settings.soniox_latency_mode;
    let mut soniox_context_text = settings.soniox_context_text;
    let mut idle_minutes = settings.idle_minutes;
    let mut show_cost_meter = settings.show_cost_meter;
    let anthropic_configured_for_reformat = settings.anthropic_configured;
    let mut reformat_model_config_id = settings.reformat_model_config_id;
    let mut auto_format_enabled = settings.auto_format_enabled;
    let mut format_nth = settings.format_nth;
    let mut format_depth = settings.format_depth;
    let mut format_context_depth = settings.format_context_depth;
    let mut format_on_stop = settings.format_on_stop;
    let mut auto_format_in_conversation = settings.auto_format_in_conversation;

    // Lazy-loaded model list. The drawer hits `/models` on first
    // mount; if the proxy is offline we just render an empty list
    // and keep the cookie value the user already had. Errors are
    // logged to console and surfaced as a hint under the dropdown.
    let mut available_models = use_signal(Vec::<DrawerModelSummary>::new);
    let mut models_error: Signal<Option<String>> = use_signal(|| None);
    use_future(move || async move {
        match fetch_models().await {
            Ok(models) => {
                // Cookie migration: if the active reformat model id
                // doesn't match any registry config but matches a
                // legacy raw model_name (`claude-haiku-4-5-20251001`,
                // `claude-sonnet-4-6`), translate to the registry id
                // that wraps that model_name. Spec §3, §8.
                let current = reformat_model_config_id.peek().clone();
                let id_match = models.iter().any(|m| m.id == current);
                if !id_match {
                    if let Some(matched) = models.iter().find(|m| m.model_name == current) {
                        web_sys::console::log_1(
                            &format!(
                                "[parley] reformat model migrated: '{current}' -> '{}'",
                                matched.id
                            )
                            .into(),
                        );
                        reformat_model_config_id.set(matched.id.clone());
                    } else if current.is_empty()
                        && let Some(first_anthropic) =
                            models.iter().find(|m| m.provider == "anthropic")
                    {
                        // No prior pick at all: default to the first
                        // Anthropic registry config.
                        reformat_model_config_id.set(first_anthropic.id.clone());
                    }
                }
                available_models.set(models);
            }
            Err(e) => {
                web_sys::console::warn_1(
                    &format!("[parley] settings drawer /models failed: {e}").into(),
                );
                models_error.set(Some(e));
            }
        }
    });
    let mut speaker1_name = settings.speaker1_name;
    let mut speaker1_source = settings.speaker1_source;
    let mut speaker2_enabled = settings.speaker2_enabled;
    let mut speaker2_name = settings.speaker2_name;
    let mut speaker2_source = settings.speaker2_source;
    let mut show_labels = settings.show_labels;
    let mut show_timestamps = settings.show_timestamps;

    if !show_settings() {
        return rsx! {};
    }

    rsx! {
        div {
            class: "settings-overlay",
            onclick: move |_| show_settings.set(false),
        }
        div { class: "settings-drawer",
            h2 { "Settings" }

            // ── API keys (proxy-managed) ──────────────────────
            h3 { class: "settings-section-heading", "API Keys" }
            SecretsKeyRow {
                provider: "assemblyai",
                label: "AssemblyAI API Key",
                hint: "Used for live transcription.",
                configured: assemblyai_configured(),
                on_changed: move |_| secrets_refresh.refresh(),
            }
            SecretsKeyRow {
                provider: "soniox",
                label: "Soniox API Key",
                hint: "Used for diarized live transcription.",
                configured: soniox_configured(),
                on_changed: move |_| secrets_refresh.refresh(),
            }
            SecretsKeyRow {
                provider: "anthropic",
                label: "Anthropic API Key",
                hint: "Used for paragraph detection and conversation mode.",
                configured: anthropic_configured(),
                on_changed: move |_| secrets_refresh.refresh(),
            }
            SecretsKeyRow {
                provider: "elevenlabs",
                label: "ElevenLabs API Key",
                hint: "Used for spoken AI replies in Conversation Mode. Optional.",
                configured: elevenlabs_configured(),
                on_changed: move |_| secrets_refresh.refresh(),
            }
            SecretsKeyRow {
                provider: "xai",
                label: "xAI API Key",
                hint: "Used for grok-stt transcription and grok-tts spoken replies. Optional.",
                configured: xai_configured(),
                on_changed: move |_| secrets_refresh.refresh(),
            }
            SecretsKeyRow {
                provider: "cartesia",
                label: "Cartesia API Key",
                hint: "Used for expressive Sonic-3 spoken replies in Conversation Mode. Optional.",
                configured: cartesia_configured(),
                on_changed: move |_| secrets_refresh.refresh(),
            }

            // ── Pipeline section ─────────────────────────────
            h3 { class: "settings-section-heading", "Pipeline" }
            p { class: "settings-hint",
                "Choose the provider and voice for spoken AI replies."
            }

            label { r#for: "pipeline-tts", "Text-to-speech" }
            div { class: "settings-row",
                select {
                    id: "pipeline-tts",
                    class: "settings-input",
                    value: "{pipeline_tts_provider}",
                    onchange: move |evt: Event<FormData>| {
                        let new_provider = evt.value();
                        if new_provider != *pipeline_tts_provider.peek() {
                            pipeline_tts_voice.set(String::new());
                        }
                        pipeline_tts_provider.set(new_provider);
                    },
                    option { value: "elevenlabs", "ElevenLabs" }
                    option { value: "xai", "xAI (grok-tts)" }
                    option { value: "cartesia", "Cartesia (Sonic-3)" }
                    option { value: "off", "Off (text only)" }
                }
            }
            p { class: "settings-hint",
                "Used in Conversation Mode for spoken replies. \
                 Requires the matching API key above (or set Off for text-only)."
            }

            if pipeline_tts_provider() != "off" {
                label { r#for: "pipeline-voice", "Voice" }
                div { class: "settings-row",
                    crate::ui::voice_picker::VoicePicker {
                        provider_id: ReadSignal::from(pipeline_tts_provider),
                        credential: ReadSignal::from(voice_credential),
                        selected_voice_id: pipeline_tts_voice,
                    }
                }
                p { class: "settings-hint",
                    "Voices come from the selected TTS provider's catalog."
                }
            }

            // ── Reformatting (global, both Transcribe + Conversation) ─
            // Spec `docs/global-reformat-spec.md` §4. Visible whenever
            // an Anthropic credential is configured; non-Anthropic
            // models in the dropdown render with a warning row until
            // the proxy gains support.
            if anthropic_configured_for_reformat() {
                h3 { class: "settings-section-heading", "Reformatting" }
                p { class: "settings-hint",
                    "Cleans up STT punctuation, capitalization, and acronyms. \
                     Used in both Transcribe Mode (every Nth turn + on stop) \
                     and Conversation Mode (each spoken user turn before send)."
                }

                label { r#for: "reformat-model", "Reformat model" }
                select {
                    id: "reformat-model",
                    class: "settings-input",
                    value: "{reformat_model_config_id}",
                    onchange: move |evt: Event<FormData>| {
                        reformat_model_config_id.set(evt.value());
                    },
                    if reformat_model_config_id().is_empty() {
                        option { value: "", "(no model selected)" }
                    }
                    for m in available_models().iter() {
                        option { key: "{m.id}", value: "{m.id}",
                            "{m.id} ({m.provider})"
                        }
                    }
                }
                {
                    let current = reformat_model_config_id();
                    let selected = available_models()
                        .iter()
                        .find(|m| m.id == current)
                        .cloned();
                    rsx! {
                        if let Some(m) = selected {
                            if m.provider != "anthropic" {
                                p { class: "settings-hint settings-warn",
                                    "\u{26a0} Provider '{m.provider}' is not yet supported by /format — \
                                     this model will return 501 until support lands."
                                }
                            }
                        } else if !current.is_empty() {
                            p { class: "settings-hint settings-warn",
                                "\u{26a0} Selected model '{current}' was not found in the proxy registry."
                            }
                        }
                    }
                }
                if let Some(err) = models_error() {
                    p { class: "settings-hint settings-warn",
                        "Couldn't load /models: {err}"
                    }
                }

                label { class: "option-row settings-option-row",
                    input {
                        r#type: "checkbox",
                        checked: "{auto_format_enabled}",
                        onchange: move |evt: Event<FormData>| {
                            auto_format_enabled.set(evt.checked());
                        },
                    }
                    "Auto-format every Nth turn (Transcribe Mode)"
                }
                if auto_format_enabled() {
                    label { r#for: "reformat-nth", "N (every Nth turn)" }
                    input {
                        id: "reformat-nth",
                        r#type: "number",
                        class: "settings-input",
                        min: "1",
                        max: "20",
                        value: "{format_nth}",
                        oninput: move |evt: Event<FormData>| {
                            if let Ok(v) = evt.value().parse::<u32>() {
                                format_nth.set(v.max(1));
                            }
                        },
                    }
                }

                label { r#for: "reformat-depth", "Reformat depth (chunks)" }
                input {
                    id: "reformat-depth",
                    r#type: "number",
                    class: "settings-input",
                    min: "1",
                    max: "6",
                    value: "{format_depth}",
                    oninput: move |evt: Event<FormData>| {
                        if let Ok(v) = evt.value().parse::<usize>() {
                            format_depth.set(v.clamp(1, 6));
                        }
                    },
                }

                label { r#for: "reformat-ctx-depth", "Additional visibility depth (chunks)" }
                input {
                    id: "reformat-ctx-depth",
                    r#type: "number",
                    class: "settings-input",
                    min: "1",
                    max: "6",
                    value: "{format_context_depth}",
                    oninput: move |evt: Event<FormData>| {
                        if let Ok(v) = evt.value().parse::<usize>() {
                            format_context_depth.set(v.clamp(1, 6));
                        }
                    },
                }
                p { class: "settings-hint",
                    "Depth 0 = full transcript. Visibility adds read-only context chunks before the editable window."
                }

                label { class: "option-row settings-option-row",
                    input {
                        r#type: "checkbox",
                        checked: "{format_on_stop}",
                        onchange: move |evt: Event<FormData>| {
                            format_on_stop.set(evt.checked());
                        },
                    }
                    "Also format on stop (full Sonnet pass)"
                }

                label { class: "option-row settings-option-row",
                    input {
                        r#type: "checkbox",
                        checked: "{auto_format_in_conversation}",
                        onchange: move |evt: Event<FormData>| {
                            auto_format_in_conversation.set(evt.checked());
                        },
                    }
                    "Auto-reformat each user voice turn (Conversation Mode)"
                }
                p { class: "settings-hint",
                    "Cleans up STT punctuation and acronyms before sending each spoken turn to the conversational LLM."
                }
            }

            // ── Language models (read-only info) ─────────────
            h3 { class: "settings-section-heading", "Language models" }
            p { class: "settings-hint",
                "Conversation replies use the persona/model picked in the conversation view; \
                 reformatting (above) uses its own model picker."
            }

            label { r#for: "stt-provider", "Speech-to-text provider" }
            div { class: "settings-row",
                crate::ui::stt_provider_picker::SttProviderPicker {
                    selected_provider_id: pipeline_stt_provider,
                }
            }
            p { class: "settings-hint",
                "Used by Conversation Mode voice input. Capture Mode supports AssemblyAI and Soniox today."
            }

            if pipeline_stt_provider() == "soniox" {
                label { r#for: "soniox-latency-mode", "Soniox latency" }
                select {
                    id: "soniox-latency-mode",
                    class: "settings-input",
                    value: "{soniox_latency_mode}",
                    onchange: move |evt: Event<FormData>| {
                        let value = evt.value();
                        let mode = soniox_latency_mode_from_value(&value);
                        let stored = mode.storage_value().to_string();
                        soniox_latency_mode.set(stored.clone());
                        save(SONIOX_LATENCY_MODE_COOKIE, &stored);
                    },
                    option { value: "fast", "Fast" }
                    option { value: "balanced", "Balanced" }
                    option { value: "careful", "Careful" }
                }
                p { class: "settings-hint",
                    "Soniox-only endpoint and finalization timing."
                }

                label { r#for: "soniox-context-text", "Soniox context" }
                textarea {
                    id: "soniox-context-text",
                    class: "settings-input soniox-context-input",
                    rows: "4",
                    value: "{soniox_context_text}",
                    oninput: move |evt: Event<FormData>| {
                        let value = evt.value();
                        soniox_context_text.set(value.clone());
                        save_local(SONIOX_CONTEXT_TEXT_STORAGE_KEY, &value);
                    },
                }
                p { class: "settings-hint",
                    "Soniox-only recognition context. Use domain, topic, names, vocabulary, or a sample sentence; it biases recognition and punctuation, but does not revise already-final text using future speech."
                }
            }

            label { r#for: "idle-timeout", "Idle timeout (minutes)" }
            input {
                id: "idle-timeout",
                r#type: "number",
                class: "settings-input",
                min: "1",
                max: "60",
                value: "{idle_minutes}",
                oninput: move |evt: Event<FormData>| {
                    if let Ok(v) = evt.value().parse::<u32>() {
                        idle_minutes.set(v);
                        save("parley_idle_minutes", &v.to_string());
                    }
                },
            }

            p { class: "settings-hint",
                "Auto-disconnect after this many minutes of silence to save costs."
            }

            // ── Cost meter toggle ────────────────────────────
            label { class: "option-row settings-option-row",
                input {
                    r#type: "checkbox",
                    checked: "{show_cost_meter}",
                    onchange: move |evt: Event<FormData>| {
                        let v = evt.checked();
                        show_cost_meter.set(v);
                        save("parley_show_cost_meter", if v { "true" } else { "false" });
                    },
                }
                "Show cost meter"
            }
            p { class: "settings-hint",
                "Display a running estimate of API costs (STT + LLM) in the status bar."
            }

            // ── Speakers section ────────────────────────────
            div { class: "settings-section-header", "Speakers" }

            // Speaker 1
            div { class: "speaker-card",
                div { class: "speaker-card-title", "Speaker 1 (You)" }
                div { class: "speaker-card-row",
                    div { class: "speaker-field",
                        label { "Name" }
                        input {
                            r#type: "text",
                            class: "settings-input",
                            value: "{speaker1_name}",
                            oninput: move |evt: Event<FormData>| {
                                let val = evt.value();
                                speaker1_name.set(val.clone());
                                save("parley_speaker1_name", &val);
                            },
                        }
                    }
                    div { class: "speaker-field",
                        label { "Source" }
                        select {
                            class: "settings-input",
                            value: "{speaker1_source}",
                            onchange: move |evt: Event<FormData>| {
                                let val = evt.value();
                                speaker1_source.set(val.clone());
                                save("parley_speaker1_source", &val);
                            },
                            option { value: "mic", "Microphone" }
                            option { value: "system", "System Audio" }
                        }
                    }
                }
            }

            // Speaker 2
            div { class: "speaker-card",
                div { class: "speaker-card-title",
                    span { "Speaker 2 (Remote)" }
                    label { class: "toggle-switch",
                        input {
                            r#type: "checkbox",
                            checked: "{speaker2_enabled}",
                            onchange: move |evt: Event<FormData>| {
                                let v = evt.checked();
                                speaker2_enabled.set(v);
                                save("parley_speaker2_enabled", if v { "true" } else { "false" });
                            },
                        }
                        span { class: "toggle-slider" }
                    }
                }
                if (speaker2_enabled)() {
                    div { class: "speaker-card-row",
                        div { class: "speaker-field",
                            label { "Name" }
                            input {
                                r#type: "text",
                                class: "settings-input",
                                value: "{speaker2_name}",
                                oninput: move |evt: Event<FormData>| {
                                    let val = evt.value();
                                    speaker2_name.set(val.clone());
                                    save("parley_speaker2_name", &val);
                                },
                            }
                        }
                        div { class: "speaker-field",
                            label { "Source" }
                            select {
                                class: "settings-input",
                                value: "{speaker2_source}",
                                onchange: move |evt: Event<FormData>| {
                                    let val = evt.value();
                                    speaker2_source.set(val.clone());
                                    save("parley_speaker2_source", &val);
                                },
                                option { value: "mic", "Microphone" }
                                option { value: "system", "System Audio" }
                            }
                        }
                    }

                    // Options
                    div { class: "speaker-options",
                        label { class: "option-row",
                            input {
                                r#type: "checkbox",
                                checked: "{show_labels}",
                                onchange: move |evt: Event<FormData>| {
                                    let v = evt.checked();
                                    show_labels.set(v);
                                    save("parley_show_labels", if v { "true" } else { "false" });
                                },
                            }
                            "Speaker labels ([Name] prefix)"
                        }
                        label { class: "option-row",
                            input {
                                r#type: "checkbox",
                                checked: "{show_timestamps}",
                                onchange: move |evt: Event<FormData>| {
                                    let v = evt.checked();
                                    show_timestamps.set(v);
                                    save("parley_show_timestamps", if v { "true" } else { "false" });
                                },
                            }
                            "Timestamps ([MM:SS] prefix)"
                        }
                    }

                    p { class: "settings-hint settings-warn",
                        "\u{26a0} Two speakers = 2\u{00d7} AssemblyAI usage"
                    }
                }
            }

            button {
                class: "btn btn-close-settings",
                onclick: move |_| show_settings.set(false),
                "Close"
            }
        }
    }
}
