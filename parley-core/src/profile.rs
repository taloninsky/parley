//! User-profile shapes — STT/TTS provider + voice + audio config.
//!
//! Profiles bundle the per-session settings that the orchestrator
//! reads when it constructs the live capture/synthesis pipeline:
//! which provider to use, which credential to draw from, which voice
//! to render in. The TOML layout matches `docs/xai-speech-integration-spec.md` §9.1.
//!
//! These types are *just* the wire/disk shape. Validation against
//! the live provider registry (rejecting `stt.provider = "made-up"`)
//! happens at the proxy boundary, where the registry actually lives —
//! `parley-core` is WASM-clean and has no view of the proxy's
//! `ProviderId` enum.

use serde::{Deserialize, Serialize};

/// Top-level profile bundle. Loaded from a TOML file under
/// `profiles/<name>.toml`; snapshotted into a session's provenance
/// when a session is created so subsequent profile edits don't
/// retroactively rewrite history.
///
/// Spec: `docs/xai-speech-integration-spec.md` §9.1.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Profile {
    /// Speech-to-text settings.
    pub stt: SttConfig,
    /// Text-to-speech settings.
    pub tts: TtsConfig,
}

/// STT block of a [`Profile`]. The `provider` string MUST match a
/// registry row whose category is `stt`; the loader enforces that.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SttConfig {
    /// Provider id, e.g. `"xai"` or `"assemblyai"`.
    pub provider: String,
    /// Named credential to draw the provider's API key from.
    /// Defaults to `"default"` when omitted.
    #[serde(default = "default_credential")]
    pub credential: String,
    /// BCP-47 language hint. Optional; provider-specific behavior
    /// when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Whether to request speaker diarization. Defaults to `true`
    /// for parity with the conversation pipeline.
    #[serde(default = "default_true")]
    pub diarize: bool,
}

/// TTS block of a [`Profile`]. The `provider` string MUST match a
/// registry row whose category is `tts`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TtsConfig {
    /// Provider id, e.g. `"xai"` or `"elevenlabs"`.
    pub provider: String,
    /// Named credential to draw the provider's API key from.
    /// Defaults to `"default"`.
    #[serde(default = "default_credential")]
    pub credential: String,
    /// Provider-native voice id (e.g. xAI `"eve"`, ElevenLabs 20-char id).
    pub voice_id: String,
    /// BCP-47 language hint. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Container/codec hint, e.g. `"mp3"`. Today only `mp3` is
    /// implemented across providers.
    #[serde(default = "default_codec")]
    pub codec: String,
}

fn default_credential() -> String {
    "default".to_string()
}

fn default_true() -> bool {
    true
}

fn default_codec() -> String {
    "mp3".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_parses_spec_example_toml() {
        let toml_str = r#"
            [stt]
            provider = "xai"
            credential = "default"
            language = "en"
            diarize = true

            [tts]
            provider = "xai"
            credential = "default"
            voice_id = "eve"
            language = "en"
            codec = "mp3"
        "#;
        let p: Profile = toml::from_str(toml_str).expect("parse profile");
        assert_eq!(p.stt.provider, "xai");
        assert_eq!(p.stt.credential, "default");
        assert_eq!(p.stt.language.as_deref(), Some("en"));
        assert!(p.stt.diarize);
        assert_eq!(p.tts.provider, "xai");
        assert_eq!(p.tts.voice_id, "eve");
        assert_eq!(p.tts.codec, "mp3");
    }

    #[test]
    fn profile_applies_defaults_when_optional_fields_omitted() {
        let toml_str = r#"
            [stt]
            provider = "assemblyai"

            [tts]
            provider = "elevenlabs"
            voice_id = "c6SfcYrb2t09NHXiT80T"
        "#;
        let p: Profile = toml::from_str(toml_str).expect("parse profile");
        assert_eq!(p.stt.credential, "default");
        assert!(p.stt.diarize, "diarize defaults to true");
        assert!(p.stt.language.is_none());
        assert_eq!(p.tts.credential, "default");
        assert_eq!(p.tts.codec, "mp3");
        assert!(p.tts.language.is_none());
    }

    #[test]
    fn profile_round_trips_through_serde_json() {
        let p = Profile {
            stt: SttConfig {
                provider: "xai".into(),
                credential: "work".into(),
                language: Some("en-US".into()),
                diarize: false,
            },
            tts: TtsConfig {
                provider: "elevenlabs".into(),
                credential: "default".into(),
                voice_id: "abc".into(),
                language: None,
                codec: "mp3".into(),
            },
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: Profile = serde_json::from_str(&s).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn profile_rejects_missing_required_fields() {
        // `tts.voice_id` has no default — the loader must refuse.
        let toml_str = r#"
            [stt]
            provider = "xai"

            [tts]
            provider = "xai"
        "#;
        let res: Result<Profile, _> = toml::from_str(toml_str);
        assert!(res.is_err(), "missing voice_id must fail to parse");
    }
}
