//! Persona — a reusable agent character: system prompt, voice, model tiers.
//!
//! Spec reference: `docs/conversation-mode-spec.md` §6.1 *Persona schema*.
//!
//! ## Design notes
//!
//! - Personas are **orthogonal to model configs** (spec §3.3). A persona
//!   says *what character to be*; a model config says *which LLM to use*.
//!   Personas reference models by id; the actual `ModelConfig` lookup
//!   happens in the `PersonaRegistry` (proxy-side) at session start.
//! - The system prompt may be either inline text or a reference to a
//!   prompt file under `~/.parley/prompts/<name>.md`. Either form
//!   round-trips through serde; resolving the file is the proxy's job.
//! - **Tiers** (`heavy` required, `fast` optional) anticipate the v2
//!   multi-tier orchestration described in §3.2 and §4.2. v1 only ever
//!   uses `heavy`. Both tiers exist in the schema today so personas
//!   written now do not need to be rewritten when v2 lands.
//! - The expression-annotation toggle (`use_expression_annotations`,
//!   spec §6.4) is here because it is per-persona policy, not per-tier.
//! - Context-management knobs (spec §9.2) are per-persona because
//!   compaction policy is part of the persona's character (a long-context
//!   theological scholar may want very late compaction; a chatty assistant
//!   may compact aggressively).

use serde::{Deserialize, Serialize};

use crate::model_config::ModelConfigId;

/// Stable identifier for a persona; matches the file stem on disk
/// (`~/.parley/personas/<id>.toml`).
pub type PersonaId = String;

/// A persona's system prompt. Either inline text or a reference to a
/// markdown file under `~/.parley/prompts/`. The proxy-side loader
/// resolves `File` references at startup; downstream code consumes the
/// already-resolved string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SystemPrompt {
    /// Inline prompt text — suitable for short prompts kept right next to
    /// the persona definition.
    Inline {
        /// The prompt text as written.
        text: String,
    },
    /// Reference to a prompt file by name. Resolved against the
    /// `~/.parley/prompts/` directory by the proxy loader.
    File {
        /// Filename (with or without `.md` extension) under
        /// `~/.parley/prompts/`.
        file: String,
    },
}

/// One tier of a persona — the LLM + voice + TTS engine pairing for a
/// single role in multi-tier orchestration.
///
/// v1 only ever activates `heavy`. `fast` exists in the schema so that
/// personas written today remain valid when v2's host/expert handoff
/// (spec §4.2) is implemented.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersonaTier {
    /// Reference to a `ModelConfig` by id. Validated against the
    /// `ModelRegistry` at registry-build time; no validation here.
    pub model_config: ModelConfigId,
    /// Provider-qualified voice identifier in the form
    /// `"<provider>:<voice_id>"`, e.g., `"elevenlabs:rachel"`. Free-form
    /// here; the TTS provider implementation parses it.
    pub voice: String,
    /// Engine identifier for the chosen TTS provider. v1 expects
    /// `"eleven_v3"` for ElevenLabs; the field exists for forward
    /// compatibility with future engines and providers.
    pub tts_model: String,
    /// How this tier introduces itself or hands off in multi-tier
    /// narration (spec §4.2). Unused in v1; recorded for v2.
    #[serde(default)]
    pub narration_style: Option<String>,
}

/// TTS-related per-persona settings. See spec §6.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersonaTtsSettings {
    /// Whether responses are spoken by default when this persona becomes
    /// active. Users can toggle TTS independently per session; this is
    /// the *default*.
    #[serde(default = "default_speak_responses_true")]
    pub default_speak_responses: bool,
    /// Whether the orchestrator auto-prepends the canonical
    /// expression-annotation instruction (spec §6.4) to this persona's
    /// system prompt at dispatch time. Personas needing custom guidance
    /// set this to `false` and write their own instruction inline.
    #[serde(default = "default_use_expression_annotations_true")]
    pub use_expression_annotations: bool,
}

fn default_speak_responses_true() -> bool {
    true
}
fn default_use_expression_annotations_true() -> bool {
    true
}

impl Default for PersonaTtsSettings {
    fn default() -> Self {
        Self {
            default_speak_responses: true,
            use_expression_annotations: true,
        }
    }
}

/// Context-window management settings for this persona (spec §9.2).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PersonaContextSettings {
    /// Trigger compaction when projected token usage for the next
    /// request would exceed this percentage of the active model's
    /// context window. Range 1–100. Default 70.
    #[serde(default = "default_compact_at_token_pct")]
    pub compact_at_token_pct: u8,
    /// Always preserve at least this many of the most recent turns
    /// uncompacted. Default 6.
    #[serde(default = "default_preserve_recent_turns")]
    pub preserve_recent_turns: u8,
}

fn default_compact_at_token_pct() -> u8 {
    70
}
fn default_preserve_recent_turns() -> u8 {
    6
}

impl Default for PersonaContextSettings {
    fn default() -> Self {
        Self {
            compact_at_token_pct: default_compact_at_token_pct(),
            preserve_recent_turns: default_preserve_recent_turns(),
        }
    }
}

/// A reusable agent character. See spec §6.1 for the on-disk shape.
///
/// `tiers.heavy` is required; `tiers.fast` is optional and reserved for
/// v2 multi-tier orchestration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Persona {
    /// Stable id; matches the file stem on disk.
    pub id: PersonaId,
    /// Human-readable name for the persona (used as the AI agent's
    /// label in transcripts).
    pub name: String,
    /// Free-form description, surfaced in the persona picker UI.
    #[serde(default)]
    pub description: String,
    /// The persona's system prompt — inline or file-referenced.
    pub system_prompt: SystemPrompt,
    /// Tier configurations. `heavy` is required; `fast` is reserved
    /// for v2 multi-tier orchestration and ignored by v1 even when
    /// present.
    pub tiers: PersonaTiers,
    /// TTS-related settings.
    #[serde(default)]
    pub tts: PersonaTtsSettings,
    /// Context-window management.
    #[serde(default)]
    pub context: PersonaContextSettings,
}

/// The tier table for a persona. `heavy` is the v1 active tier; `fast`
/// is reserved for v2's host/anchor model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersonaTiers {
    /// The expert / heavy-model tier. Required. v1 always dispatches
    /// turns to this tier.
    pub heavy: PersonaTier,
    /// The fast / host tier. Optional in v1 (ignored if present);
    /// becomes the anchor model in v2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast: Option<PersonaTier>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_heavy_tier() -> PersonaTier {
        PersonaTier {
            model_config: "claude-opus-latest".into(),
            voice: "elevenlabs:rachel".into(),
            tts_model: "eleven_v3".into(),
            narration_style: Some("concise".into()),
        }
    }

    fn sample_persona() -> Persona {
        Persona {
            id: "theology-scholar".into(),
            name: "Theological Scholar".into(),
            description: "Bible study assistant.".into(),
            system_prompt: SystemPrompt::File {
                file: "theology-scholar.md".into(),
            },
            tiers: PersonaTiers {
                heavy: sample_heavy_tier(),
                fast: None,
            },
            tts: PersonaTtsSettings::default(),
            context: PersonaContextSettings::default(),
        }
    }

    #[test]
    fn defaults_match_spec() {
        let tts = PersonaTtsSettings::default();
        assert!(tts.default_speak_responses);
        assert!(tts.use_expression_annotations);
        let ctx = PersonaContextSettings::default();
        assert_eq!(ctx.compact_at_token_pct, 70);
        assert_eq!(ctx.preserve_recent_turns, 6);
    }

    #[test]
    fn json_roundtrip_with_file_prompt_preserves_persona() {
        let original = sample_persona();
        let json = serde_json::to_string(&original).expect("serialize");
        let back: Persona = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, back);
    }

    #[test]
    fn json_roundtrip_with_inline_prompt_preserves_persona() {
        let mut p = sample_persona();
        p.system_prompt = SystemPrompt::Inline {
            text: "You are a careful exegete.".into(),
        };
        let json = serde_json::to_string(&p).expect("serialize");
        let back: Persona = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(p, back);
    }

    #[test]
    fn fast_tier_omitted_when_none() {
        let p = sample_persona();
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(
            !json.contains("\"fast\""),
            "absent fast tier must not serialize: {json}"
        );
    }

    #[test]
    fn fast_tier_round_trips_when_present() {
        let mut p = sample_persona();
        p.tiers.fast = Some(PersonaTier {
            model_config: "claude-haiku-latest".into(),
            voice: "elevenlabs:adam".into(),
            tts_model: "eleven_v3".into(),
            narration_style: Some("anchor".into()),
        });
        let json = serde_json::to_string(&p).expect("serialize");
        let back: Persona = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(p, back);
    }

    // ── TOML round-trips matching the spec §6.1 shape ──────────────

    #[test]
    fn toml_full_persona_round_trips() {
        // The on-disk shape wraps the persona under `[persona]` so the
        // file is self-describing at a glance.
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct File {
            persona: Persona,
        }
        let original = File {
            persona: sample_persona(),
        };
        let serialized = toml::to_string(&original).expect("serialize toml");
        let back: File = toml::from_str(&serialized).expect("deserialize toml");
        assert_eq!(original, back);
    }

    #[test]
    fn toml_minimal_persona_uses_defaults() {
        // The minimum a user has to write: id, name, prompt, heavy tier.
        // Everything else falls back to defaults.
        let toml_text = r#"
            [persona]
            id = "minimal"
            name = "Minimal"
            system_prompt = { text = "be helpful" }

            [persona.tiers.heavy]
            model_config = "claude-haiku-latest"
            voice = "elevenlabs:rachel"
            tts_model = "eleven_v3"
        "#;
        #[derive(Deserialize)]
        struct File {
            persona: Persona,
        }
        let f: File = toml::from_str(toml_text).expect("parse");
        assert_eq!(f.persona.id, "minimal");
        assert_eq!(f.persona.description, "");
        assert!(f.persona.tiers.fast.is_none());
        assert!(f.persona.tts.default_speak_responses);
        assert_eq!(f.persona.context.compact_at_token_pct, 70);
        assert!(matches!(
            f.persona.system_prompt,
            SystemPrompt::Inline { .. }
        ));
    }

    #[test]
    fn toml_persona_missing_heavy_tier_fails() {
        // The heavy tier is required; a config that omits it should
        // not silently load with garbage.
        let toml_text = r#"
            [persona]
            id = "broken"
            name = "Broken"
            system_prompt = { text = "x" }
            [persona.tiers]
        "#;
        #[derive(Deserialize)]
        struct File {
            #[allow(dead_code)]
            persona: Persona,
        }
        let result: Result<File, _> = toml::from_str(toml_text);
        assert!(result.is_err(), "missing heavy tier must fail to parse");
    }
}
