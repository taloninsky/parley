//! Provider registry — the canonical, closed set of external providers
//! Parley knows how to talk to.
//!
//! Spec reference: `docs/secrets-storage-spec.md` §4.1.
//!
//! This module is **pure data**. No I/O, no env-var reads, no keystore
//! access. The registry is the single source of truth for:
//!
//! - Which providers exist.
//! - Which category each one belongs to (STT / LLM / TTS).
//! - The stable lowercase id used in URLs and keystore accounts.
//! - The display name shown in the UI.
//! - The env-var name that overrides the `default` credential.
//!
//! Adding a provider is one entry in [`REGISTRY`] plus a variant in
//! [`ProviderId`]. The HTTP API and UI auto-pick it up.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Coarse categorization of providers by the role they play in the
/// pipeline. The category drives UI grouping and tells the orchestrator
/// which provider slots are interchangeable.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderCategory {
    /// Speech-to-text providers (AssemblyAI, Deepgram, local Whisper, …).
    Stt,
    /// General-purpose language models used by formatting *and*
    /// conversation (Anthropic, OpenAI, local Ollama, …).
    Llm,
    /// Text-to-speech providers (ElevenLabs, OpenAI TTS, Piper, …).
    Tts,
}

impl ProviderCategory {
    /// All categories, in display order.
    pub const fn all() -> &'static [ProviderCategory] {
        &[
            ProviderCategory::Stt,
            ProviderCategory::Llm,
            ProviderCategory::Tts,
        ]
    }

    /// Stable lowercase string form (used in JSON and URLs).
    pub const fn as_str(self) -> &'static str {
        match self {
            ProviderCategory::Stt => "stt",
            ProviderCategory::Llm => "llm",
            ProviderCategory::Tts => "tts",
        }
    }
}

impl fmt::Display for ProviderCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The closed set of providers Parley knows about. Each variant has a
/// stable lowercase string id; see [`ProviderId::as_str`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderId {
    /// Anthropic — used for conversation and formatting LLM calls.
    Anthropic,
    /// AssemblyAI — used for streaming STT and temporary token issuance.
    #[serde(rename = "assemblyai")]
    AssemblyAi,
    /// ElevenLabs — used for streaming TTS in Conversation Mode.
    #[serde(rename = "elevenlabs")]
    ElevenLabs,
    /// xAI — used for streaming STT (`grok-stt`) and TTS with a single
    /// bearer token covering both surfaces. Spec: `docs/xai-speech-integration-spec.md`.
    Xai,
}

impl ProviderId {
    /// All known providers, in display order.
    pub const fn all() -> &'static [ProviderId] {
        &[
            ProviderId::Anthropic,
            ProviderId::AssemblyAi,
            ProviderId::ElevenLabs,
            ProviderId::Xai,
        ]
    }

    /// Stable lowercase string id (used in keystore accounts and URLs).
    pub const fn as_str(self) -> &'static str {
        self.descriptor().id
    }

    /// Categories this provider belongs to. Most providers have exactly
    /// one; xAI has two (STT + TTS) because a single bearer token serves
    /// both surfaces. See `docs/xai-speech-integration-spec.md` §6.1.1.
    pub const fn categories(self) -> &'static [ProviderCategory] {
        self.descriptor().categories
    }

    /// `true` if this provider plays in the given category.
    pub fn has_category(self, cat: ProviderCategory) -> bool {
        self.categories().contains(&cat)
    }

    /// Human-readable display name (shown in the UI).
    pub const fn display_name(self) -> &'static str {
        self.descriptor().display_name
    }

    /// Environment variable that, when set, takes precedence over the
    /// `default` credential for this provider. Other named credentials
    /// ignore the env var.
    pub const fn env_var(self) -> &'static str {
        self.descriptor().env_var
    }

    /// Walk the static [`REGISTRY`] for this id. Panics only if the
    /// registry is internally inconsistent — covered by a unit test.
    pub const fn descriptor(self) -> &'static ProviderDescriptor {
        // const fn — manual lookup since we can't iterate slices in const.
        let idx = self as usize;
        &REGISTRY[idx]
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProviderId {
    type Err = UnknownProvider;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for p in ProviderId::all() {
            if p.as_str() == s {
                return Ok(*p);
            }
        }
        Err(UnknownProvider(s.to_string()))
    }
}

/// Returned when a string does not match any known [`ProviderId`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownProvider(pub String);

impl fmt::Display for UnknownProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown provider: {}", self.0)
    }
}

impl std::error::Error for UnknownProvider {}

/// Static metadata for one provider. The variant order in [`ProviderId`]
/// must match the index order in [`REGISTRY`] — enforced by
/// `registry_index_matches_variants`.
#[derive(Debug)]
pub struct ProviderDescriptor {
    /// Stable lowercase string id.
    pub id: &'static str,
    /// Human-readable name.
    pub display_name: &'static str,
    /// Categories this provider plays in. Usually one; multi-category
    /// providers (e.g. xAI: STT + TTS under one bearer token) list all
    /// the categories they participate in.
    pub categories: &'static [ProviderCategory],
    /// Env var that overrides the `default` credential.
    pub env_var: &'static str,
}

/// The full canonical provider list.
///
/// **Order must match the discriminant order of [`ProviderId`].** A
/// unit test enforces this so a future variant addition doesn't
/// silently alias an unrelated descriptor.
pub static REGISTRY: &[ProviderDescriptor] = &[
    ProviderDescriptor {
        id: "anthropic",
        display_name: "Anthropic",
        categories: &[ProviderCategory::Llm],
        env_var: "PARLEY_ANTHROPIC_API_KEY",
    },
    ProviderDescriptor {
        id: "assemblyai",
        display_name: "AssemblyAI",
        categories: &[ProviderCategory::Stt],
        env_var: "PARLEY_ASSEMBLYAI_API_KEY",
    },
    ProviderDescriptor {
        id: "elevenlabs",
        display_name: "ElevenLabs",
        categories: &[ProviderCategory::Tts],
        env_var: "PARLEY_ELEVENLABS_API_KEY",
    },
    ProviderDescriptor {
        id: "xai",
        display_name: "xAI",
        categories: &[ProviderCategory::Stt, ProviderCategory::Tts],
        env_var: "PARLEY_XAI_API_KEY",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_index_matches_variants() {
        // Each ProviderId discriminant must equal its index in REGISTRY,
        // because `descriptor()` does `&REGISTRY[self as usize]`.
        for (i, p) in ProviderId::all().iter().enumerate() {
            assert_eq!(
                *p as usize, i,
                "ProviderId variant order must match REGISTRY index order"
            );
        }
        assert_eq!(
            ProviderId::all().len(),
            REGISTRY.len(),
            "every ProviderId must have a REGISTRY entry"
        );
    }

    #[test]
    fn provider_ids_round_trip_through_string() {
        for p in ProviderId::all() {
            let s = p.as_str();
            let parsed: ProviderId = s.parse().expect("must round-trip");
            assert_eq!(parsed, *p, "round-trip failed for {s}");
        }
    }

    #[test]
    fn provider_ids_serialize_to_lowercase_string() {
        let json = serde_json::to_string(&ProviderId::Anthropic).unwrap();
        assert_eq!(json, "\"anthropic\"");
        let json = serde_json::to_string(&ProviderId::AssemblyAi).unwrap();
        assert_eq!(json, "\"assemblyai\"");
        let json = serde_json::to_string(&ProviderId::ElevenLabs).unwrap();
        assert_eq!(json, "\"elevenlabs\"");
        let json = serde_json::to_string(&ProviderId::Xai).unwrap();
        assert_eq!(json, "\"xai\"");
    }

    #[test]
    fn provider_ids_deserialize_from_lowercase_string() {
        let p: ProviderId = serde_json::from_str("\"anthropic\"").unwrap();
        assert_eq!(p, ProviderId::Anthropic);
        let p: ProviderId = serde_json::from_str("\"assemblyai\"").unwrap();
        assert_eq!(p, ProviderId::AssemblyAi);
        let p: ProviderId = serde_json::from_str("\"elevenlabs\"").unwrap();
        assert_eq!(p, ProviderId::ElevenLabs);
        let p: ProviderId = serde_json::from_str("\"xai\"").unwrap();
        assert_eq!(p, ProviderId::Xai);
    }

    #[test]
    fn unknown_provider_string_rejected() {
        let err = "openai".parse::<ProviderId>().unwrap_err();
        assert_eq!(err, UnknownProvider("openai".into()));
    }

    #[test]
    fn empty_string_rejected() {
        assert!("".parse::<ProviderId>().is_err());
    }

    #[test]
    fn every_provider_has_distinct_id_and_env_var() {
        let ids: Vec<&str> = REGISTRY.iter().map(|d| d.id).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "duplicate provider id in REGISTRY");

        let envs: Vec<&str> = REGISTRY.iter().map(|d| d.env_var).collect();
        let mut sorted = envs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), envs.len(), "duplicate env var in REGISTRY");
    }

    #[test]
    fn every_provider_belongs_to_a_known_category() {
        let valid = ProviderCategory::all();
        for d in REGISTRY {
            assert!(
                !d.categories.is_empty(),
                "{} must declare at least one category",
                d.id
            );
            for cat in d.categories {
                assert!(valid.contains(cat), "{} has unknown category", d.id);
            }
        }
    }

    #[test]
    fn category_strings_are_stable() {
        assert_eq!(ProviderCategory::Stt.as_str(), "stt");
        assert_eq!(ProviderCategory::Llm.as_str(), "llm");
        assert_eq!(ProviderCategory::Tts.as_str(), "tts");
    }

    #[test]
    fn descriptor_lookup_matches_explicit_metadata() {
        assert_eq!(
            ProviderId::Anthropic.categories(),
            &[ProviderCategory::Llm]
        );
        assert_eq!(
            ProviderId::AssemblyAi.categories(),
            &[ProviderCategory::Stt]
        );
        assert_eq!(
            ProviderId::ElevenLabs.categories(),
            &[ProviderCategory::Tts]
        );
        assert_eq!(ProviderId::Anthropic.display_name(), "Anthropic");
        assert_eq!(ProviderId::AssemblyAi.display_name(), "AssemblyAI");
        assert_eq!(ProviderId::Xai.display_name(), "xAI");
        assert_eq!(ProviderId::Anthropic.env_var(), "PARLEY_ANTHROPIC_API_KEY");
        assert_eq!(
            ProviderId::AssemblyAi.env_var(),
            "PARLEY_ASSEMBLYAI_API_KEY"
        );
        assert_eq!(ProviderId::Xai.env_var(), "PARLEY_XAI_API_KEY");
    }

    #[test]
    fn xai_is_multi_category_stt_and_tts() {
        let cats = ProviderId::Xai.categories();
        assert!(cats.contains(&ProviderCategory::Stt));
        assert!(cats.contains(&ProviderCategory::Tts));
        assert!(!cats.contains(&ProviderCategory::Llm));
        assert!(ProviderId::Xai.has_category(ProviderCategory::Stt));
        assert!(ProviderId::Xai.has_category(ProviderCategory::Tts));
        assert!(!ProviderId::Xai.has_category(ProviderCategory::Llm));
    }

    #[test]
    fn has_category_matches_categories_membership() {
        for p in ProviderId::all() {
            for cat in ProviderCategory::all() {
                assert_eq!(
                    p.has_category(*cat),
                    p.categories().contains(cat),
                    "{p}.has_category({cat}) must match categories().contains"
                );
            }
        }
    }
}
