//! Pure-function STT/TTS provider selection with the §14.1 fallback
//! chain.
//!
//! Spec: `docs/xai-speech-integration-spec.md` §6.6, §14.1.
//!
//! This module is intentionally I/O-free: every decision input —
//! profile preference, credential name, the per-category fallback
//! order — is passed in. Credential presence is queried through
//! [`SecretsManager::resolve`], which is already mockable in tests.
//!
//! The actual wiring into `OrchestratorContext` (holding the
//! `Arc<dyn SttProvider>` map, routing a capture-start to the chosen
//! provider) lands with Step 7 / Step 12 when the orchestrator's
//! dispatch loop first needs it. Keeping the selection function pure
//! lets us unit-test the fallback semantics today without standing
//! up the full orchestrator surface.

use crate::providers::{ProviderCategory, ProviderId};
use crate::secrets::{DEFAULT_CREDENTIAL, SecretsManager};

/// Outcome of [`select_stt_provider`] / [`select_tts_provider`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSelection {
    /// A provider + credential pair with a resolvable key was found.
    Use {
        /// Provider to drive.
        provider: ProviderId,
        /// Credential name the orchestrator should resolve again at
        /// call time (we intentionally don't carry the key bytes
        /// around — the `SecretsManager` is the authority).
        credential: String,
        /// `None` when `provider` matches the profile's preference;
        /// `Some(requested)` when fallback kicked in. The orchestrator
        /// surfaces this as a `ProviderFallback` log line per §14.1.
        fell_back_from: Option<ProviderId>,
    },
    /// No provider in the category has a configured credential. The
    /// orchestrator is expected to emit
    /// `OrchestratorEvent::ProviderUnconfigured { category, attempted }`
    /// and render the "configure your key" banner.
    Unconfigured {
        /// Which providers were tried, in order.
        attempted: Vec<ProviderId>,
    },
}

/// Canonical STT fallback order per §14.1:
///   `xai` → `assemblyai`.
pub const STT_FALLBACK_ORDER: &[ProviderId] =
    &[ProviderId::Xai, ProviderId::AssemblyAi];

/// Canonical TTS fallback order per §14.1:
///   `xai` → `elevenlabs`.
pub const TTS_FALLBACK_ORDER: &[ProviderId] =
    &[ProviderId::Xai, ProviderId::ElevenLabs];

/// Select the STT provider to drive for a turn.
///
/// - `preferred`: the profile's `stt.provider`.
/// - `credential`: the credential name to try for the preferred
///   provider. Falls back to [`DEFAULT_CREDENTIAL`] when empty.
/// - `secrets`: authority on whether a given credential is resolvable.
///
/// Fallback: if `preferred` has no resolvable credential, we walk
/// [`STT_FALLBACK_ORDER`] looking for the first provider whose
/// `default` credential resolves. This matches §14.1's "first
/// configured provider in this category in a fixed preference order".
pub fn select_stt_provider(
    preferred: ProviderId,
    credential: &str,
    secrets: &SecretsManager,
) -> ProviderSelection {
    select(preferred, credential, secrets, ProviderCategory::Stt, STT_FALLBACK_ORDER)
}

/// Select the TTS provider to drive for a turn. See
/// [`select_stt_provider`]; identical semantics with
/// [`TTS_FALLBACK_ORDER`].
pub fn select_tts_provider(
    preferred: ProviderId,
    credential: &str,
    secrets: &SecretsManager,
) -> ProviderSelection {
    select(preferred, credential, secrets, ProviderCategory::Tts, TTS_FALLBACK_ORDER)
}

fn select(
    preferred: ProviderId,
    credential: &str,
    secrets: &SecretsManager,
    category: ProviderCategory,
    fallback_order: &[ProviderId],
) -> ProviderSelection {
    let credential = if credential.is_empty() {
        DEFAULT_CREDENTIAL
    } else {
        credential
    };

    let preferred_valid_for_category = preferred.has_category(category);
    let mut attempted: Vec<ProviderId> = Vec::new();

    if preferred_valid_for_category {
        attempted.push(preferred);
        if secrets.resolve(preferred, credential).is_some() {
            return ProviderSelection::Use {
                provider: preferred,
                credential: credential.to_string(),
                fell_back_from: None,
            };
        }
    }

    for &candidate in fallback_order {
        if candidate == preferred {
            // Already tried above (if it was category-valid).
            continue;
        }
        if !candidate.has_category(category) {
            continue;
        }
        attempted.push(candidate);
        if secrets.resolve(candidate, DEFAULT_CREDENTIAL).is_some() {
            let fell_back_from = if preferred_valid_for_category {
                Some(preferred)
            } else {
                None
            };
            return ProviderSelection::Use {
                provider: candidate,
                credential: DEFAULT_CREDENTIAL.to_string(),
                fell_back_from,
            };
        }
    }

    ProviderSelection::Unconfigured { attempted }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::{InMemoryKeyStore, SecretsManager, StaticEnv};
    use std::sync::Arc;

    fn fresh_secrets() -> Arc<SecretsManager> {
        let store = InMemoryKeyStore::new();
        let index_path = std::env::temp_dir().join(format!(
            "parley-stt-router-test-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        Arc::new(SecretsManager::new(
            Box::new(store),
            Box::new(StaticEnv::new()),
            index_path,
        ))
    }

    fn seed(secrets: &SecretsManager, provider: ProviderId, credential: &str, key: &str) {
        secrets
            .set(provider, credential, key)
            .expect("seed credential");
    }

    // ── STT ──────────────────────────────────────────────────────────

    #[test]
    fn stt_preferred_xai_with_credential_wins() {
        let secrets = fresh_secrets();
        seed(&secrets, ProviderId::Xai, DEFAULT_CREDENTIAL, "k");
        let s = select_stt_provider(ProviderId::Xai, DEFAULT_CREDENTIAL, &secrets);
        assert_eq!(
            s,
            ProviderSelection::Use {
                provider: ProviderId::Xai,
                credential: DEFAULT_CREDENTIAL.to_string(),
                fell_back_from: None,
            }
        );
    }

    #[test]
    fn stt_preferred_xai_missing_falls_back_to_assemblyai() {
        let secrets = fresh_secrets();
        seed(&secrets, ProviderId::AssemblyAi, DEFAULT_CREDENTIAL, "aa");
        let s = select_stt_provider(ProviderId::Xai, DEFAULT_CREDENTIAL, &secrets);
        assert_eq!(
            s,
            ProviderSelection::Use {
                provider: ProviderId::AssemblyAi,
                credential: DEFAULT_CREDENTIAL.to_string(),
                fell_back_from: Some(ProviderId::Xai),
            }
        );
    }

    #[test]
    fn stt_preferred_assemblyai_with_credential_wins_no_fallback_flag() {
        let secrets = fresh_secrets();
        seed(&secrets, ProviderId::AssemblyAi, DEFAULT_CREDENTIAL, "aa");
        seed(&secrets, ProviderId::Xai, DEFAULT_CREDENTIAL, "k");
        let s = select_stt_provider(ProviderId::AssemblyAi, DEFAULT_CREDENTIAL, &secrets);
        assert_eq!(
            s,
            ProviderSelection::Use {
                provider: ProviderId::AssemblyAi,
                credential: DEFAULT_CREDENTIAL.to_string(),
                fell_back_from: None,
            }
        );
    }

    #[test]
    fn stt_no_provider_configured_returns_unconfigured_with_attempted() {
        let secrets = fresh_secrets();
        let s = select_stt_provider(ProviderId::Xai, DEFAULT_CREDENTIAL, &secrets);
        match s {
            ProviderSelection::Unconfigured { attempted } => {
                assert_eq!(attempted, vec![ProviderId::Xai, ProviderId::AssemblyAi]);
            }
            other => panic!("expected Unconfigured, got {other:?}"),
        }
    }

    #[test]
    fn stt_non_stt_preferred_skips_preference_and_falls_through() {
        // Anthropic is LLM-only: not in STT category. Prefer it ->
        // walk fallback order from the top (xAI then AssemblyAI).
        let secrets = fresh_secrets();
        seed(&secrets, ProviderId::AssemblyAi, DEFAULT_CREDENTIAL, "aa");
        let s = select_stt_provider(ProviderId::Anthropic, DEFAULT_CREDENTIAL, &secrets);
        // Because the preference wasn't a valid STT provider, the
        // outcome isn't flagged as fallback (nothing to fall back
        // *from* in the category).
        assert_eq!(
            s,
            ProviderSelection::Use {
                provider: ProviderId::AssemblyAi,
                credential: DEFAULT_CREDENTIAL.to_string(),
                fell_back_from: None,
            }
        );
    }

    #[test]
    fn stt_named_credential_is_honored_on_preferred() {
        let secrets = fresh_secrets();
        seed(&secrets, ProviderId::Xai, "work", "kw");
        let s = select_stt_provider(ProviderId::Xai, "work", &secrets);
        assert_eq!(
            s,
            ProviderSelection::Use {
                provider: ProviderId::Xai,
                credential: "work".to_string(),
                fell_back_from: None,
            }
        );
    }

    #[test]
    fn stt_empty_credential_string_falls_back_to_default() {
        let secrets = fresh_secrets();
        seed(&secrets, ProviderId::Xai, DEFAULT_CREDENTIAL, "k");
        let s = select_stt_provider(ProviderId::Xai, "", &secrets);
        match s {
            ProviderSelection::Use { credential, .. } => {
                assert_eq!(credential, DEFAULT_CREDENTIAL);
            }
            other => panic!("expected Use, got {other:?}"),
        }
    }

    #[test]
    fn stt_fallback_only_tries_default_credential_on_fallback_candidates() {
        // Even though "work" credential is present on AssemblyAI, the
        // fallback walk uses the default slot — matching §14.1's
        // "first configured provider" language which implies the
        // canonical default credential.
        let secrets = fresh_secrets();
        seed(&secrets, ProviderId::AssemblyAi, "work", "aa-work");
        let s = select_stt_provider(ProviderId::Xai, DEFAULT_CREDENTIAL, &secrets);
        match s {
            ProviderSelection::Unconfigured { attempted } => {
                assert_eq!(attempted, vec![ProviderId::Xai, ProviderId::AssemblyAi]);
            }
            other => panic!("expected Unconfigured, got {other:?}"),
        }
    }

    // ── TTS ──────────────────────────────────────────────────────────

    #[test]
    fn tts_preferred_xai_with_credential_wins() {
        let secrets = fresh_secrets();
        seed(&secrets, ProviderId::Xai, DEFAULT_CREDENTIAL, "k");
        let s = select_tts_provider(ProviderId::Xai, DEFAULT_CREDENTIAL, &secrets);
        assert_eq!(
            s,
            ProviderSelection::Use {
                provider: ProviderId::Xai,
                credential: DEFAULT_CREDENTIAL.to_string(),
                fell_back_from: None,
            }
        );
    }

    #[test]
    fn tts_preferred_xai_missing_falls_back_to_elevenlabs() {
        let secrets = fresh_secrets();
        seed(&secrets, ProviderId::ElevenLabs, DEFAULT_CREDENTIAL, "el");
        let s = select_tts_provider(ProviderId::Xai, DEFAULT_CREDENTIAL, &secrets);
        assert_eq!(
            s,
            ProviderSelection::Use {
                provider: ProviderId::ElevenLabs,
                credential: DEFAULT_CREDENTIAL.to_string(),
                fell_back_from: Some(ProviderId::Xai),
            }
        );
    }

    #[test]
    fn tts_no_provider_configured_returns_unconfigured() {
        let secrets = fresh_secrets();
        let s = select_tts_provider(ProviderId::Xai, DEFAULT_CREDENTIAL, &secrets);
        match s {
            ProviderSelection::Unconfigured { attempted } => {
                assert_eq!(attempted, vec![ProviderId::Xai, ProviderId::ElevenLabs]);
            }
            other => panic!("expected Unconfigured, got {other:?}"),
        }
    }

    #[test]
    fn tts_assemblyai_is_not_in_tts_category_so_ignored() {
        // AssemblyAI has only ProviderCategory::Stt. Preferring it for
        // TTS must not consume the preference — we fall straight
        // through to the TTS fallback order.
        let secrets = fresh_secrets();
        seed(&secrets, ProviderId::AssemblyAi, DEFAULT_CREDENTIAL, "aa");
        seed(&secrets, ProviderId::ElevenLabs, DEFAULT_CREDENTIAL, "el");
        let s = select_tts_provider(ProviderId::AssemblyAi, DEFAULT_CREDENTIAL, &secrets);
        assert_eq!(
            s,
            ProviderSelection::Use {
                provider: ProviderId::ElevenLabs,
                credential: DEFAULT_CREDENTIAL.to_string(),
                fell_back_from: None,
            }
        );
    }

    #[test]
    fn fallback_orders_match_spec() {
        assert_eq!(
            STT_FALLBACK_ORDER,
            &[ProviderId::Xai, ProviderId::AssemblyAi]
        );
        assert_eq!(
            TTS_FALLBACK_ORDER,
            &[ProviderId::Xai, ProviderId::ElevenLabs]
        );
    }
}
