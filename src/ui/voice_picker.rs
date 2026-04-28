//! Cross-provider voice picker.
//!
//! Fetches `GET /api/tts/voices?provider={id}&credential={cred}` on
//! mount and re-fetches whenever `provider_id` or `credential`
//! changes. Renders a `<select>` keyed off the returned
//! `VoiceDescriptor.id` and writes the selection back into the
//! caller-owned `selected_voice_id` signal.
//!
//! Spec: `docs/xai-speech-integration-spec.md` §9.2.

use dioxus::prelude::*;
use serde::Deserialize;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

const API_BASE: &str = "http://127.0.0.1:3033/api/tts";

/// Wire shape of one voice returned by `/api/tts/voices`. Mirrors
/// `parley_core::tts::VoiceDescriptor` but redeclared here to keep
/// the WASM crate decoupled from native-only code paths.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct VoiceDescriptor {
    /// Provider-native voice id (sent back on synthesis).
    pub id: String,
    /// Human-readable label shown in the dropdown.
    pub display_name: String,
    /// BCP-47 language tags (advisory; surfaced as a tooltip).
    #[serde(default)]
    pub language_tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct VoicesResponse {
    voices: Vec<VoiceDescriptor>,
}

/// Fetch the voice catalog for `provider` / `credential`. Returns the
/// HTTP error message on failure so the picker can surface it inline.
pub async fn fetch_voices(
    provider: &str,
    credential: &str,
) -> Result<Vec<VoiceDescriptor>, String> {
    let window = web_sys::window().ok_or("no window")?;
    let opts = web_sys::RequestInit::new();
    opts.set_method("GET");
    let url = format!("{API_BASE}/voices?provider={provider}&credential={credential}");
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
    let parsed: VoicesResponse =
        serde_json::from_str(&json_str).map_err(|e| format!("decode: {e}"))?;
    Ok(parsed.voices)
}

/// Voice picker component. The caller owns `selected_voice_id` and
/// reads it after the user picks. When the catalog finishes loading,
/// if the current selection isn't in the returned list the picker
/// auto-selects the first entry so the synthesis path always has a
/// resolvable voice id.
#[component]
pub fn VoicePicker(
    /// TTS provider id (e.g. `"xai"`, `"elevenlabs"`).
    provider_id: ReadSignal<String>,
    /// Named credential (typically `"default"`).
    credential: ReadSignal<String>,
    /// Current selection; mutated when the user picks.
    selected_voice_id: Signal<String>,
) -> Element {
    let voices = use_resource(move || async move {
        let p = provider_id();
        let c = credential();
        web_sys::console::log_1(
            &format!("[voice-picker] fetching voices: provider={p}, credential={c}").into(),
        );
        fetch_voices(&p, &c).await
    });

    use_effect(move || {
        let mut sel = selected_voice_id;
        if let Some(Ok(list)) = voices.read().as_ref() {
            if list.is_empty() {
                web_sys::console::log_1(&"[voice-picker] effect: empty list".into());
                return;
            }
            let current = sel.peek().clone();
            let in_list = list.iter().any(|v| v.id == current);
            web_sys::console::log_1(
                &format!(
                    "[voice-picker] effect: current={current:?}, in_list={in_list}, list_ids={:?}",
                    list.iter().map(|v| &v.id).collect::<Vec<_>>(),
                )
                .into(),
            );
            if !in_list {
                let selected = list[0].id.clone();
                gloo_timers::callback::Timeout::new(0, move || {
                    sel.set(selected);
                })
                .forget();
            }
        } else {
            web_sys::console::log_1(&"[voice-picker] effect: voices not ready".into());
        }
    });

    rsx! {
        match voices.read().as_ref() {
            None => rsx! { span { class: "voice-picker__status", "Loading voices…" } },
            Some(Err(e)) => rsx! {
                span { class: "voice-picker__error", "Voice list unavailable: {e}" }
            },
            Some(Ok(list)) if list.is_empty() => rsx! {
                span { class: "voice-picker__status", "No voices available." }
            },
            Some(Ok(list)) => {
                let current = selected_voice_id.read().clone();
                let options: Vec<VoiceDescriptor> = list.clone();
                rsx! {
                    select {
                        class: "settings-input voice-picker__select",
                        // Note: the `selected` attr on each <option> is the
                        // load-bearing thing here. Setting `value` on a
                        // <select> alone is unreliable in Dioxus when options
                        // come from a `for` loop — the value can be applied
                        // before the matching <option> exists, so the select
                        // silently falls back to the first option.
                        value: "{current}",
                        onchange: move |evt| {
                            let v = evt.value();
                            web_sys::console::log_1(
                                &format!("[voice-picker] onchange: {v}").into(),
                            );
                            selected_voice_id.set(v);
                        },
                        for v in options {
                            option {
                                key: "{v.id}",
                                value: "{v.id}",
                                selected: v.id == current,
                                title: if v.language_tags.is_empty() {
                                    String::new()
                                } else {
                                    v.language_tags.join(", ")
                                },
                                "{v.display_name}"
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voices_response_parses_minimal_json() {
        let json = r#"{
            "provider": "xai",
            "voices": [
                {"id": "eve", "display_name": "Eve", "language_tags": ["en-US"]},
                {"id": "ara", "display_name": "Ara"}
            ]
        }"#;
        let parsed: VoicesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.voices.len(), 2);
        assert_eq!(parsed.voices[0].id, "eve");
        assert_eq!(parsed.voices[0].display_name, "Eve");
        assert_eq!(parsed.voices[0].language_tags, vec!["en-US"]);
        assert_eq!(parsed.voices[1].id, "ara");
        assert!(parsed.voices[1].language_tags.is_empty());
    }

    #[test]
    fn voices_response_rejects_when_voices_field_missing() {
        let json = r#"{"provider": "xai"}"#;
        assert!(serde_json::from_str::<VoicesResponse>(json).is_err());
    }
}
