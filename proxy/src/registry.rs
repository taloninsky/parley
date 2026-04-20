//! Filesystem loaders for `~/.parley/models/*.toml` and
//! `~/.parley/personas/*.toml`. Lives in the proxy because the WASM
//! frontend has no filesystem; the data types themselves live in
//! `parley-core`.
//!
//! Spec references:
//! - `docs/conversation-mode-spec.md` §6.1 (personas), §6.2 (models)
//!
//! ## Validation policy
//!
//! - Each TOML file is parsed independently. A bad file produces a
//!   diagnostic that names the file (and the underlying parser
//!   message) and is reported alongside the successful loads — one
//!   bad file does not abort the load.
//! - Persona-to-model references are validated **after** all files are
//!   parsed. A persona that references an unknown model is reported as
//!   a `RegistryError::UnknownModelRef` and is **excluded** from the
//!   loaded registry, but other personas continue to load. The caller
//!   decides whether a config error is fatal or merely loud.
//! - File stems must match the `id` field inside the TOML so the
//!   filesystem and in-memory keys stay in sync.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use parley_core::model_config::{ModelConfig, ModelConfigId};
use parley_core::persona::{Persona, PersonaId, SystemPrompt};
use thiserror::Error;

/// Result of loading a registry. `entries` is the successfully-loaded
/// map; `errors` is per-file or per-reference diagnostics that the
/// caller should log. Both fields can be non-empty simultaneously.
#[derive(Debug)]
pub struct LoadResult<K, V> {
    /// Successfully loaded entries, keyed by id.
    pub entries: HashMap<K, V>,
    /// Per-file or per-reference errors. Always reported, never
    /// fatal — the caller decides whether to abort.
    pub errors: Vec<RegistryError>,
}

impl<K, V> LoadResult<K, V> {
    /// Convenience for tests / boot logs: total entries loaded.
    pub fn count(&self) -> usize {
        self.entries.len()
    }
}

/// All loadable failure modes. Each variant carries enough context to
/// log a useful one-line diagnostic.
#[derive(Debug, Error)]
pub enum RegistryError {
    /// I/O error while listing or reading a file. Wraps the path so the
    /// log line tells the user *which* file.
    #[error("I/O error reading {path}: {source}")]
    Io {
        /// Filesystem path that failed.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// TOML parse error for a specific file.
    #[error("invalid TOML in {path}: {source}")]
    Toml {
        /// Filesystem path of the offending file.
        path: PathBuf,
        /// Underlying parser error.
        #[source]
        source: Box<toml::de::Error>,
    },
    /// `id` field inside the file did not match the file stem on disk.
    #[error("id mismatch in {path}: file stem is '{stem}' but file declares id '{id}'")]
    IdMismatch {
        /// Filesystem path of the offending file.
        path: PathBuf,
        /// File stem (filename without extension).
        stem: String,
        /// `id` declared inside the TOML.
        id: String,
    },
    /// A persona referenced a model id that no loaded model has. The
    /// persona is excluded from the resulting registry.
    #[error(
        "persona '{persona}' references unknown model id '{model}' in tier '{tier}' \
         (no such file under models/)"
    )]
    UnknownModelRef {
        /// Offending persona id.
        persona: PersonaId,
        /// Tier that holds the bad reference (`"heavy"` or `"fast"`).
        tier: String,
        /// Model id that was not found.
        model: ModelConfigId,
    },
    /// A persona's `system_prompt = { file = "..." }` referenced a file
    /// that does not exist under `~/.parley/prompts/`. The persona is
    /// excluded from the resulting registry.
    #[error("persona '{persona}': system prompt file '{file}' not found in {dir}")]
    MissingPromptFile {
        /// Offending persona id.
        persona: PersonaId,
        /// File reference as written in the persona TOML.
        file: String,
        /// Directory searched.
        dir: PathBuf,
    },
}

/// File wrapper for `[model]` TOML.
#[derive(serde::Deserialize)]
struct ModelFile {
    model: ModelConfig,
}

/// File wrapper for `[persona]` TOML.
#[derive(serde::Deserialize)]
struct PersonaFile {
    persona: Persona,
}

/// Load every `*.toml` in `dir` as a `ModelConfig`. A non-existent
/// directory is treated as "no model configs" (empty result, no
/// errors) so that fresh installs do not fail.
pub fn load_model_configs(dir: &Path) -> LoadResult<ModelConfigId, ModelConfig> {
    load_dir(dir, "toml", |path, contents| {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let parsed: ModelFile = toml::from_str(contents).map_err(|e| RegistryError::Toml {
            path: path.to_path_buf(),
            source: Box::new(e),
        })?;
        if parsed.model.id != stem {
            return Err(RegistryError::IdMismatch {
                path: path.to_path_buf(),
                stem,
                id: parsed.model.id,
            });
        }
        Ok((parsed.model.id.clone(), parsed.model))
    })
}

/// Load every `*.toml` in `personas_dir` as a `Persona`, validating
/// that:
/// 1. each persona's tiers reference known models (passed in via
///    `models`);
/// 2. `system_prompt = { file = "..." }` references resolve under
///    `prompts_dir` (the file is checked for existence — its contents
///    are NOT slurped here; that happens at orchestrator dispatch).
///
/// Personas that fail validation are excluded from the result and
/// reported via the `errors` vec.
pub fn load_personas(
    personas_dir: &Path,
    prompts_dir: &Path,
    models: &HashMap<ModelConfigId, ModelConfig>,
) -> LoadResult<PersonaId, Persona> {
    let mut raw = load_dir(personas_dir, "toml", |path, contents| {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let parsed: PersonaFile = toml::from_str(contents).map_err(|e| RegistryError::Toml {
            path: path.to_path_buf(),
            source: Box::new(e),
        })?;
        if parsed.persona.id != stem {
            return Err(RegistryError::IdMismatch {
                path: path.to_path_buf(),
                stem,
                id: parsed.persona.id,
            });
        }
        Ok((parsed.persona.id.clone(), parsed.persona))
    });

    // Cross-reference validation: model refs and prompt files. A
    // persona that fails any check is dropped and a diagnostic is
    // appended.
    raw.entries.retain(|id, persona| {
        let mut ok = true;
        if !models.contains_key(&persona.tiers.heavy.model_config) {
            raw.errors.push(RegistryError::UnknownModelRef {
                persona: id.clone(),
                tier: "heavy".into(),
                model: persona.tiers.heavy.model_config.clone(),
            });
            ok = false;
        }
        if let Some(fast) = &persona.tiers.fast
            && !models.contains_key(&fast.model_config)
        {
            raw.errors.push(RegistryError::UnknownModelRef {
                persona: id.clone(),
                tier: "fast".into(),
                model: fast.model_config.clone(),
            });
            ok = false;
        }
        if let SystemPrompt::File { file } = &persona.system_prompt {
            let candidate = prompt_file_candidate(prompts_dir, file);
            if !candidate.exists() {
                raw.errors.push(RegistryError::MissingPromptFile {
                    persona: id.clone(),
                    file: file.clone(),
                    dir: prompts_dir.to_path_buf(),
                });
                ok = false;
            }
        }
        ok
    });

    raw
}

/// Generic directory-walking loader. Skips entries that are not files,
/// not the right extension, or whose name starts with a `.` (so editor
/// swap files like `.persona.toml.swp` are ignored).
fn load_dir<K, V>(
    dir: &Path,
    extension: &str,
    mut parse_one: impl FnMut(&Path, &str) -> Result<(K, V), RegistryError>,
) -> LoadResult<K, V>
where
    K: std::hash::Hash + Eq,
{
    let mut out = LoadResult {
        entries: HashMap::new(),
        errors: Vec::new(),
    };
    let read_dir = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Treat a missing directory as "no entries". This is the
            // expected fresh-install state.
            return out;
        }
        Err(e) => {
            out.errors.push(RegistryError::Io {
                path: dir.to_path_buf(),
                source: e,
            });
            return out;
        }
    };

    for entry in read_dir.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some(extension) {
            continue;
        }
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'))
        {
            continue;
        }
        let contents = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                out.errors.push(RegistryError::Io {
                    path: path.clone(),
                    source: e,
                });
                continue;
            }
        };
        match parse_one(&path, &contents) {
            Ok((k, v)) => {
                out.entries.insert(k, v);
            }
            Err(e) => out.errors.push(e),
        }
    }
    out
}

/// Resolve a `system_prompt.file` reference against the prompts
/// directory, accepting both `"name"` and `"name.md"` forms.
fn prompt_file_candidate(prompts_dir: &Path, file: &str) -> PathBuf {
    if file.contains('.') {
        prompts_dir.join(file)
    } else {
        prompts_dir.join(format!("{file}.md"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;

    fn write(dir: &Path, name: &str, contents: &str) {
        let mut f = File::create(dir.join(name)).expect("create");
        f.write_all(contents.as_bytes()).expect("write");
    }

    fn valid_model_toml(id: &str) -> String {
        format!(
            r#"
            [model]
            id = "{id}"
            provider = "anthropic"
            model_name = "claude-x"
            context_window = 200000
            "#
        )
    }

    fn persona_toml(id: &str, model_ref: &str, prompt: &str) -> String {
        format!(
            r#"
            [persona]
            id = "{id}"
            name = "{id}"
            system_prompt = {{ text = "{prompt}" }}

            [persona.tiers.heavy]
            model_config = "{model_ref}"
            voice = "elevenlabs:rachel"
            tts_model = "eleven_v3"
            "#
        )
    }

    // ── Model loader ──────────────────────────────────────────────

    #[test]
    fn missing_models_dir_returns_empty_no_errors() {
        let tmp = TempDir::new().unwrap();
        let absent = tmp.path().join("does-not-exist");
        let r = load_model_configs(&absent);
        assert_eq!(r.count(), 0);
        assert!(r.errors.is_empty());
    }

    #[test]
    fn loads_well_formed_model_files() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "alpha.toml", &valid_model_toml("alpha"));
        write(tmp.path(), "beta.toml", &valid_model_toml("beta"));
        let r = load_model_configs(tmp.path());
        assert_eq!(r.count(), 2);
        assert!(r.errors.is_empty());
        assert!(r.entries.contains_key("alpha"));
        assert!(r.entries.contains_key("beta"));
    }

    #[test]
    fn ignores_non_toml_files_and_dotfiles() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "real.toml", &valid_model_toml("real"));
        write(tmp.path(), "readme.md", "irrelevant");
        write(tmp.path(), ".real.toml.swp", "editor swap");
        let r = load_model_configs(tmp.path());
        assert_eq!(r.count(), 1);
        assert!(r.errors.is_empty());
    }

    #[test]
    fn id_mismatch_is_reported_and_skipped() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "named-this.toml", &valid_model_toml("but-this"));
        let r = load_model_configs(tmp.path());
        assert_eq!(r.count(), 0);
        assert_eq!(r.errors.len(), 1);
        assert!(matches!(
            r.errors[0],
            RegistryError::IdMismatch { ref stem, ref id, .. }
                if stem == "named-this" && id == "but-this"
        ));
    }

    #[test]
    fn malformed_toml_is_reported_and_others_still_load() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "good.toml", &valid_model_toml("good"));
        write(tmp.path(), "bad.toml", "this { is not = valid toml");
        let r = load_model_configs(tmp.path());
        assert_eq!(r.count(), 1);
        assert!(r.entries.contains_key("good"));
        assert_eq!(r.errors.len(), 1);
        assert!(matches!(r.errors[0], RegistryError::Toml { .. }));
    }

    // ── Persona loader ────────────────────────────────────────────

    #[test]
    fn persona_with_known_model_and_inline_prompt_loads() {
        let tmp = TempDir::new().unwrap();
        let models_dir = tmp.path().join("models");
        let personas_dir = tmp.path().join("personas");
        let prompts_dir = tmp.path().join("prompts");
        fs::create_dir_all(&models_dir).unwrap();
        fs::create_dir_all(&personas_dir).unwrap();
        fs::create_dir_all(&prompts_dir).unwrap();

        write(&models_dir, "m1.toml", &valid_model_toml("m1"));
        write(
            &personas_dir,
            "p1.toml",
            &persona_toml("p1", "m1", "be helpful"),
        );

        let models = load_model_configs(&models_dir);
        let personas = load_personas(&personas_dir, &prompts_dir, &models.entries);
        assert!(personas.errors.is_empty(), "errors: {:?}", personas.errors);
        assert_eq!(personas.count(), 1);
        assert!(personas.entries.contains_key("p1"));
    }

    #[test]
    fn persona_referencing_unknown_model_is_excluded_with_diagnostic() {
        let tmp = TempDir::new().unwrap();
        let models_dir = tmp.path().join("models");
        let personas_dir = tmp.path().join("personas");
        let prompts_dir = tmp.path().join("prompts");
        fs::create_dir_all(&models_dir).unwrap();
        fs::create_dir_all(&personas_dir).unwrap();
        fs::create_dir_all(&prompts_dir).unwrap();

        write(
            &personas_dir,
            "ghost.toml",
            &persona_toml("ghost", "no-such-model", "x"),
        );

        let models = load_model_configs(&models_dir);
        let personas = load_personas(&personas_dir, &prompts_dir, &models.entries);
        assert_eq!(personas.count(), 0);
        assert_eq!(personas.errors.len(), 1);
        assert!(matches!(
            personas.errors[0],
            RegistryError::UnknownModelRef { ref persona, ref tier, ref model }
                if persona == "ghost" && tier == "heavy" && model == "no-such-model"
        ));
    }

    #[test]
    fn persona_with_file_prompt_resolved_against_prompts_dir() {
        let tmp = TempDir::new().unwrap();
        let models_dir = tmp.path().join("models");
        let personas_dir = tmp.path().join("personas");
        let prompts_dir = tmp.path().join("prompts");
        fs::create_dir_all(&models_dir).unwrap();
        fs::create_dir_all(&personas_dir).unwrap();
        fs::create_dir_all(&prompts_dir).unwrap();

        write(&models_dir, "m1.toml", &valid_model_toml("m1"));
        write(&prompts_dir, "scholar.md", "be a scholar");
        let toml = r#"
            [persona]
            id = "scholar"
            name = "Scholar"
            system_prompt = { file = "scholar" }

            [persona.tiers.heavy]
            model_config = "m1"
            voice = "elevenlabs:rachel"
            tts_model = "eleven_v3"
            "#;
        write(&personas_dir, "scholar.toml", toml);

        let models = load_model_configs(&models_dir);
        let personas = load_personas(&personas_dir, &prompts_dir, &models.entries);
        assert!(personas.errors.is_empty(), "errors: {:?}", personas.errors);
        assert_eq!(personas.count(), 1);
    }

    #[test]
    fn persona_with_missing_prompt_file_is_excluded_with_diagnostic() {
        let tmp = TempDir::new().unwrap();
        let models_dir = tmp.path().join("models");
        let personas_dir = tmp.path().join("personas");
        let prompts_dir = tmp.path().join("prompts");
        fs::create_dir_all(&models_dir).unwrap();
        fs::create_dir_all(&personas_dir).unwrap();
        fs::create_dir_all(&prompts_dir).unwrap();

        write(&models_dir, "m1.toml", &valid_model_toml("m1"));
        let toml = r#"
            [persona]
            id = "noprompt"
            name = "NoPrompt"
            system_prompt = { file = "missing.md" }

            [persona.tiers.heavy]
            model_config = "m1"
            voice = "elevenlabs:rachel"
            tts_model = "eleven_v3"
        "#;
        write(&personas_dir, "noprompt.toml", toml);

        let models = load_model_configs(&models_dir);
        let personas = load_personas(&personas_dir, &prompts_dir, &models.entries);
        assert_eq!(personas.count(), 0);
        assert_eq!(personas.errors.len(), 1);
        assert!(matches!(
            personas.errors[0],
            RegistryError::MissingPromptFile { ref persona, .. } if persona == "noprompt"
        ));
    }

    #[test]
    fn persona_with_bad_fast_tier_model_excludes_persona() {
        let tmp = TempDir::new().unwrap();
        let models_dir = tmp.path().join("models");
        let personas_dir = tmp.path().join("personas");
        let prompts_dir = tmp.path().join("prompts");
        fs::create_dir_all(&models_dir).unwrap();
        fs::create_dir_all(&personas_dir).unwrap();
        fs::create_dir_all(&prompts_dir).unwrap();

        write(&models_dir, "heavy.toml", &valid_model_toml("heavy"));
        let toml = r#"
            [persona]
            id = "two-tier"
            name = "TwoTier"
            system_prompt = { text = "x" }

            [persona.tiers.heavy]
            model_config = "heavy"
            voice = "elevenlabs:rachel"
            tts_model = "eleven_v3"

            [persona.tiers.fast]
            model_config = "missing-fast"
            voice = "elevenlabs:adam"
            tts_model = "eleven_v3"
        "#;
        write(&personas_dir, "two-tier.toml", toml);

        let models = load_model_configs(&models_dir);
        let personas = load_personas(&personas_dir, &prompts_dir, &models.entries);
        assert_eq!(personas.count(), 0);
        assert!(personas.errors.iter().any(|e| matches!(
            e,
            RegistryError::UnknownModelRef { tier, .. } if tier == "fast"
        )));
    }

    #[test]
    fn good_personas_load_alongside_bad_ones() {
        let tmp = TempDir::new().unwrap();
        let models_dir = tmp.path().join("models");
        let personas_dir = tmp.path().join("personas");
        let prompts_dir = tmp.path().join("prompts");
        fs::create_dir_all(&models_dir).unwrap();
        fs::create_dir_all(&personas_dir).unwrap();
        fs::create_dir_all(&prompts_dir).unwrap();

        write(&models_dir, "m1.toml", &valid_model_toml("m1"));
        write(&personas_dir, "good.toml", &persona_toml("good", "m1", "x"));
        write(
            &personas_dir,
            "bad.toml",
            &persona_toml("bad", "missing", "x"),
        );

        let models = load_model_configs(&models_dir);
        let personas = load_personas(&personas_dir, &prompts_dir, &models.entries);
        assert_eq!(personas.count(), 1);
        assert!(personas.entries.contains_key("good"));
        assert_eq!(personas.errors.len(), 1);
    }
}
