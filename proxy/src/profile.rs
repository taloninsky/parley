//! Profile loading and registry validation.
//!
//! `parley_core::profile::Profile` defines the on-disk shape; this
//! module is the proxy-side boundary that binds those raw strings
//! to live [`ProviderId`] values via the [`REGISTRY`]. A profile that
//! names a provider not in the registry — or a provider that doesn't
//! play in the expected category — is rejected at load time with
//! [`ProfileError::UnknownProvider`].
//!
//! Spec: `docs/xai-speech-integration-spec.md` §9.1.

use parley_core::profile::Profile;
use thiserror::Error;

use crate::providers::{ProviderCategory, ProviderId, UnknownProvider};

/// Failure modes returned by [`validate_profile`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProfileError {
    /// The string in `[stt|tts].provider` does not match any
    /// [`ProviderId`] *in the expected category*. Carries enough
    /// detail for a UI to surface a precise inline error.
    #[error("unknown provider for {category}: {value}")]
    UnknownProvider {
        /// `"stt"` or `"tts"` — which block of the profile failed.
        category: &'static str,
        /// Verbatim string from the TOML.
        value: String,
    },
}

/// Walk the parsed [`Profile`] and verify both `stt.provider` and
/// `tts.provider` reference real providers that play in the right
/// category. Returns the resolved [`ProviderId`]s on success so the
/// caller doesn't have to re-parse them.
pub fn validate_profile(profile: &Profile) -> Result<ResolvedProviders, ProfileError> {
    let stt = resolve_in_category(&profile.stt.provider, ProviderCategory::Stt, "stt")?;
    let tts = resolve_in_category(&profile.tts.provider, ProviderCategory::Tts, "tts")?;
    Ok(ResolvedProviders { stt, tts })
}

/// Output of [`validate_profile`]: the typed provider ids the rest
/// of the proxy can hand straight to a `Box<dyn SttProvider>` /
/// `Box<dyn TtsProvider>` factory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedProviders {
    /// Resolved STT provider.
    pub stt: ProviderId,
    /// Resolved TTS provider.
    pub tts: ProviderId,
}

fn resolve_in_category(
    raw: &str,
    expected: ProviderCategory,
    block: &'static str,
) -> Result<ProviderId, ProfileError> {
    let id: ProviderId =
        raw.parse()
            .map_err(|UnknownProvider(v)| ProfileError::UnknownProvider {
                category: block,
                value: v,
            })?;
    if !id.has_category(expected) {
        return Err(ProfileError::UnknownProvider {
            category: block,
            value: raw.to_string(),
        });
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use parley_core::profile::{SttConfig, TtsConfig};

    fn profile_with(stt_provider: &str, tts_provider: &str) -> Profile {
        Profile {
            stt: SttConfig {
                provider: stt_provider.into(),
                credential: "default".into(),
                language: None,
                diarize: true,
            },
            tts: TtsConfig {
                provider: tts_provider.into(),
                credential: "default".into(),
                voice_id: "eve".into(),
                language: None,
                codec: "mp3".into(),
            },
        }
    }

    #[test]
    fn validate_accepts_xai_xai() {
        let p = profile_with("xai", "xai");
        let r = validate_profile(&p).expect("xai/xai is valid");
        assert_eq!(r.stt, ProviderId::Xai);
        assert_eq!(r.tts, ProviderId::Xai);
    }

    #[test]
    fn validate_accepts_assemblyai_elevenlabs() {
        let p = profile_with("assemblyai", "elevenlabs");
        let r = validate_profile(&p).expect("assemblyai/elevenlabs is valid");
        assert_eq!(r.stt, ProviderId::AssemblyAi);
        assert_eq!(r.tts, ProviderId::ElevenLabs);
    }

    #[test]
    fn validate_rejects_unknown_stt_provider() {
        let p = profile_with("made-up", "xai");
        match validate_profile(&p).unwrap_err() {
            ProfileError::UnknownProvider { category, value } => {
                assert_eq!(category, "stt");
                assert_eq!(value, "made-up");
            }
        }
    }

    #[test]
    fn validate_rejects_unknown_tts_provider() {
        let p = profile_with("xai", "made-up");
        match validate_profile(&p).unwrap_err() {
            ProfileError::UnknownProvider { category, value } => {
                assert_eq!(category, "tts");
                assert_eq!(value, "made-up");
            }
        }
    }

    #[test]
    fn validate_rejects_llm_provider_in_stt_slot() {
        // Anthropic is registered but only as an LLM provider — must
        // not slip into the STT slot.
        let p = profile_with("anthropic", "xai");
        match validate_profile(&p).unwrap_err() {
            ProfileError::UnknownProvider { category, value } => {
                assert_eq!(category, "stt");
                assert_eq!(value, "anthropic");
            }
        }
    }

    #[test]
    fn validate_rejects_stt_only_provider_in_tts_slot() {
        let p = profile_with("xai", "assemblyai");
        match validate_profile(&p).unwrap_err() {
            ProfileError::UnknownProvider { category, value } => {
                assert_eq!(category, "tts");
                assert_eq!(value, "assemblyai");
            }
        }
    }
}
