//! STT provider picker.
//!
//! Sibling of [`voice_picker`](super::voice_picker). Renders the list
//! of `stt`-category providers from `/api/secrets/status`, disabling
//! entries whose `default` credential is unconfigured. Selection is
//! written into the caller-owned `selected_provider_id` signal.
//!
//! Spec: `docs/xai-speech-integration-spec.md` §9.2.1.

use dioxus::prelude::*;

use crate::ui::secrets::{ProviderStatus, SecretsStatus, use_secrets_status};

/// Returns the `stt` providers in the same order the proxy's secrets
/// status emitted them. Empty when the status hasn't loaded yet.
pub fn stt_providers(status: &SecretsStatus) -> Vec<ProviderStatus> {
    status
        .categories
        .get("stt")
        .cloned()
        .unwrap_or_default()
}

/// `true` when the provider has a resolvable `default` credential
/// (env var or keystore). Named credentials don't gate the picker —
/// the picker only chooses a provider; the credential dropdown is a
/// separate concern.
pub fn provider_is_configured(p: &ProviderStatus) -> bool {
    p.credential("default")
        .map(|c| c.configured)
        .unwrap_or(false)
}

/// STT provider picker component.
///
/// The dropdown disables rows whose `default` credential is missing
/// so the user can see at a glance which providers need a key
/// configured before they can be selected. Selecting a disabled row
/// is impossible via the native `<select>` UI, so no extra guard is
/// needed beyond the `disabled` attribute.
#[component]
pub fn SttProviderPicker(
    /// Current selection; mutated when the user picks.
    selected_provider_id: Signal<String>,
) -> Element {
    let (status, _) = use_secrets_status();

    rsx! {
        match &*status.read_unchecked() {
            None => rsx! { span { class: "stt-picker__status", "Loading providers…" } },
            Some(Err(e)) => rsx! {
                span { class: "stt-picker__error", "Provider list unavailable: {e}" }
            },
            Some(Ok(s)) => {
                let providers = stt_providers(s);
                if providers.is_empty() {
                    rsx! { span { class: "stt-picker__status", "No STT providers registered." } }
                } else {
                    let current = selected_provider_id.read().clone();
                    rsx! {
                        select {
                            class: "stt-picker__select",
                            value: "{current}",
                            onchange: move |evt| {
                                selected_provider_id.set(evt.value());
                            },
                            for p in providers {
                                option {
                                    key: "{p.id}",
                                    value: "{p.id}",
                                    disabled: !provider_is_configured(&p),
                                    title: if provider_is_configured(&p) {
                                        String::new()
                                    } else {
                                        format!(
                                            "No credential configured (set {} or store one in the keystore)",
                                            p.env_var,
                                        )
                                    },
                                    "{p.display_name}"
                                }
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
    use crate::ui::secrets::{CredentialSource, CredentialStatus};
    use std::collections::BTreeMap;

    fn provider(id: &str, env_var: &str, default_configured: bool) -> ProviderStatus {
        ProviderStatus {
            id: id.to_string(),
            display_name: id.to_string(),
            env_var: env_var.to_string(),
            credentials: vec![CredentialStatus {
                name: "default".into(),
                configured: default_configured,
                source: if default_configured {
                    Some(CredentialSource::Keystore)
                } else {
                    None
                },
                warning: None,
            }],
        }
    }

    fn status_with(stt_providers: Vec<ProviderStatus>) -> SecretsStatus {
        let mut categories = BTreeMap::new();
        categories.insert("stt".to_string(), stt_providers);
        SecretsStatus {
            categories,
            errors: vec![],
        }
    }

    #[test]
    fn stt_providers_returns_empty_when_category_missing() {
        let s = SecretsStatus {
            categories: BTreeMap::new(),
            errors: vec![],
        };
        assert!(stt_providers(&s).is_empty());
    }

    #[test]
    fn stt_providers_returns_proxy_order() {
        let s = status_with(vec![
            provider("xai", "PARLEY_XAI_API_KEY", true),
            provider("assemblyai", "PARLEY_ASSEMBLYAI_API_KEY", true),
        ]);
        let got = stt_providers(&s);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "xai");
        assert_eq!(got[1].id, "assemblyai");
    }

    #[test]
    fn provider_is_configured_reflects_default_credential_state() {
        let p_yes = provider("xai", "X", true);
        let p_no = provider("assemblyai", "Y", false);
        assert!(provider_is_configured(&p_yes));
        assert!(!provider_is_configured(&p_no));
    }

    #[test]
    fn provider_is_configured_false_when_default_missing_entirely() {
        let p = ProviderStatus {
            id: "weird".into(),
            display_name: "Weird".into(),
            env_var: "Z".into(),
            credentials: vec![],
        };
        assert!(!provider_is_configured(&p));
    }
}
