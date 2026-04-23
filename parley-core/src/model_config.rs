//! Model configuration — provider/model/options bundle referenced by personas.
//!
//! Spec reference: `docs/conversation-mode-spec.md` §6.2 *Model Configs*.
//!
//! ## Design notes
//!
//! - Lives on disk as `~/.parley/models/<id>.toml`. This crate defines only
//!   the data shape (and runs round-trip TOML tests as a dev dependency).
//!   The actual filesystem loader lives in `parley-proxy` because the WASM
//!   crate has no filesystem.
//! - Provider-specific options pass through opaquely as
//!   `serde_json::Value`. The shared `LlmProvider` trait (Conversation Mode
//!   spec §12.2) handles only universal knobs; per-provider extension traits
//!   (§12.3) consume the opaque blob. Keeping it as `Value` means adding a
//!   new option to a provider does not require touching this crate.
//! - Cost tracking (Conversation Mode spec §11) needs per-token rates;
//!   `input_rate_per_1m` and `output_rate_per_1m` live here so any
//!   `ModelConfig` is self-sufficient for cost reporting.

use serde::{Deserialize, Serialize};

use crate::tts::ChunkPolicy;

/// Stable identifier for a model configuration. Used by personas to
/// reference a model by id; must match the file stem of the TOML file
/// (e.g., `~/.parley/models/claude-opus-latest.toml` →
/// `id = "claude-opus-latest"`).
pub type ModelConfigId = String;

/// LLM provider tag. Adding a new provider is additive: a new variant
/// here and a new `LlmProvider` impl in the proxy. Sessions persisted
/// before the new variant existed continue to deserialize because
/// existing variants are not renumbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmProviderTag {
    /// Anthropic (Claude family).
    Anthropic,
    /// OpenAI (GPT family).
    Openai,
    /// Google (Gemini family).
    Google,
    /// xAI (Grok family).
    Xai,
    /// Local model server speaking the OpenAI HTTP shape (Ollama, LM
    /// Studio, vLLM, etc.). Provider extensions distinguish further.
    LocalOpenaiCompatible,
}

/// Per-1M-token pricing in USD. Both fields default to `0.0` so a
/// freshly written model file is valid even before pricing is filled in;
/// cost tracking will simply report `$0.00` until the rates are set.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TokenRates {
    /// Input (prompt) cost per 1,000,000 tokens, USD.
    #[serde(default)]
    pub input_per_1m: f64,
    /// Output (completion) cost per 1,000,000 tokens, USD.
    #[serde(default)]
    pub output_per_1m: f64,
}

impl Default for TokenRates {
    fn default() -> Self {
        Self {
            input_per_1m: 0.0,
            output_per_1m: 0.0,
        }
    }
}

/// One model configuration. Personas reference these by `id`.
///
/// On-disk shape (TOML; see Conversation Mode spec §6.2):
///
/// ```toml
/// [model]
/// id = "claude-opus-latest"
/// provider = "anthropic"
/// model_name = "claude-opus-4-7-20260301"
/// context_window = 200000
///
/// [model.rates]
/// input_per_1m = 15.0
/// output_per_1m = 75.0
///
/// [model.options]
/// temperature = 0.7
/// extended_thinking = { enabled = true, budget_tokens = 8000 }
/// ```
///
/// `options` is intentionally opaque — anything the provider accepts goes
/// here and is forwarded to the provider-specific extension trait at
/// dispatch time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Stable id; must match the file stem on disk.
    pub id: ModelConfigId,
    /// Which provider hosts this model.
    pub provider: LlmProviderTag,
    /// The provider's own identifier for the model (e.g.,
    /// `"claude-opus-4-7-20260301"`). Distinct from `id`, which is our
    /// alias.
    pub model_name: String,
    /// Total context window in tokens. Used by the compaction logic
    /// (Conversation Mode spec §9) to compute when to trigger.
    pub context_window: u32,
    /// Pricing for cost tracking. Defaults to zero rates so model files
    /// without pricing remain loadable.
    #[serde(default)]
    pub rates: TokenRates,
    /// Provider-specific options. Forwarded opaquely to the provider's
    /// extension trait. Defaults to `null`.
    #[serde(default)]
    pub options: serde_json::Value,
    /// Paragraph-bounded TTS chunking policy applied when a turn
    /// dispatched against this model speaks via TTS. Defaults to
    /// [`ChunkPolicy::default`] when the on-disk file omits the
    /// section, so existing model files load unchanged.
    ///
    /// Spec: `docs/paragraph-tts-chunking-spec.md` §3.4.
    #[serde(default)]
    pub tts_chunking: ChunkPolicy,
}

impl ModelConfig {
    /// Compute the USD cost of a single LLM call given input and output
    /// token counts. Returns `0.0` if rates are unset (the documented
    /// behaviour for a fresh model file).
    pub fn cost_usd(&self, input_tokens: u64, output_tokens: u64) -> f64 {
        let input = (input_tokens as f64) * self.rates.input_per_1m / 1_000_000.0;
        let output = (output_tokens as f64) * self.rates.output_per_1m / 1_000_000.0;
        input + output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_anthropic() -> ModelConfig {
        ModelConfig {
            id: "claude-opus-latest".into(),
            provider: LlmProviderTag::Anthropic,
            model_name: "claude-opus-4-7-20260301".into(),
            context_window: 200_000,
            rates: TokenRates {
                input_per_1m: 15.0,
                output_per_1m: 75.0,
            },
            options: serde_json::json!({
                "temperature": 0.7,
                "extended_thinking": { "enabled": true, "budget_tokens": 8000 }
            }),
            tts_chunking: ChunkPolicy::default(),
        }
    }

    #[test]
    fn cost_usd_computes_per_million_pricing() {
        let m = sample_anthropic();
        // 1M input × $15 + 1M output × $75 = $90.
        assert!((m.cost_usd(1_000_000, 1_000_000) - 90.0).abs() < 1e-9);
    }

    #[test]
    fn cost_usd_is_zero_when_rates_are_zero() {
        let mut m = sample_anthropic();
        m.rates = TokenRates::default();
        assert_eq!(m.cost_usd(123_456, 789_012), 0.0);
    }

    #[test]
    fn cost_usd_handles_partial_million() {
        let m = sample_anthropic();
        // 500k input × $15/1M + 0 = $7.50
        assert!((m.cost_usd(500_000, 0) - 7.5).abs() < 1e-9);
    }

    #[test]
    fn json_roundtrip_preserves_all_fields() {
        let original = sample_anthropic();
        let json = serde_json::to_string(&original).expect("serialize");
        let back: ModelConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, back);
    }

    #[test]
    fn provider_tags_serialize_snake_case() {
        for (tag, expected) in [
            (LlmProviderTag::Anthropic, "anthropic"),
            (LlmProviderTag::Openai, "openai"),
            (LlmProviderTag::Google, "google"),
            (LlmProviderTag::Xai, "xai"),
            (
                LlmProviderTag::LocalOpenaiCompatible,
                "local_openai_compatible",
            ),
        ] {
            let json = serde_json::to_string(&tag).expect("serialize");
            assert_eq!(json, format!("\"{expected}\""));
        }
    }

    #[test]
    fn toml_roundtrip_matches_spec_shape() {
        // This mirrors the TOML shape documented in the spec §6.2 and in
        // the doc comment on `ModelConfig`. The on-disk format wraps the
        // ModelConfig under `[model]` so users can keep multiple models
        // per file in the future if we ever want that, and so the file
        // is self-describing at a glance.
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct File {
            model: ModelConfig,
        }

        let original = File {
            model: sample_anthropic(),
        };
        let serialized = toml::to_string(&original).expect("serialize toml");
        let back: File = toml::from_str(&serialized).expect("deserialize toml");
        assert_eq!(original, back);
    }

    #[test]
    fn toml_loads_minimum_required_fields_only() {
        // Optional fields (rates, options) must default cleanly so users
        // can write the smallest possible config.
        let toml_text = r#"
            [model]
            id = "tiny"
            provider = "anthropic"
            model_name = "claude-haiku-4-5"
            context_window = 200000
        "#;
        #[derive(Deserialize)]
        struct File {
            model: ModelConfig,
        }
        let f: File = toml::from_str(toml_text).expect("parse");
        assert_eq!(f.model.id, "tiny");
        assert_eq!(f.model.rates.input_per_1m, 0.0);
        assert_eq!(f.model.rates.output_per_1m, 0.0);
        assert!(f.model.options.is_null());
        // Missing [model.tts_chunking] section must yield the default
        // policy. Existing on-disk model files predate this section
        // and must continue to load unchanged.
        assert_eq!(f.model.tts_chunking, ChunkPolicy::default());
    }

    #[test]
    fn toml_overrides_individual_chunking_fields() {
        // Users should be able to tune one knob without restating
        // every default. Verifies serde's per-field defaulting on
        // ChunkPolicy itself.
        let toml_text = r#"
            [model]
            id = "tuned"
            provider = "anthropic"
            model_name = "claude"
            context_window = 1

            [model.tts_chunking]
            paragraph_wait_ms = 5000
            hard_cap_chars = 800
        "#;
        #[derive(Deserialize)]
        struct File {
            model: ModelConfig,
        }
        let f: File = toml::from_str(toml_text).expect("parse");
        let defaults = ChunkPolicy::default();
        assert_eq!(f.model.tts_chunking.paragraph_wait_ms, 5000);
        assert_eq!(f.model.tts_chunking.hard_cap_chars, 800);
        // Untouched fields keep their defaults.
        assert_eq!(
            f.model.tts_chunking.first_chunk_max_sentences,
            defaults.first_chunk_max_sentences
        );
        assert_eq!(
            f.model.tts_chunking.idle_timeout_ms,
            defaults.idle_timeout_ms
        );
    }

    #[test]
    fn unknown_provider_tag_fails_to_deserialize() {
        // Guard against typos in user-edited config files. Better to
        // surface a clear error than silently fall back.
        let toml_text = r#"
            [model]
            id = "tiny"
            provider = "antrhopic"
            model_name = "x"
            context_window = 1
        "#;
        #[derive(Deserialize)]
        struct File {
            #[allow(dead_code)]
            model: ModelConfig,
        }
        let result: Result<File, _> = toml::from_str(toml_text);
        assert!(result.is_err(), "typo'd provider tag should fail to parse");
    }
}
