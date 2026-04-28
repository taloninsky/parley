//! Shared settings state lifted to [`crate::ui::root::Root`] so the
//! gear button + settings drawer can render at the top-level shell
//! and stay reachable from both the Transcribe and Conversation
//! views.
//!
//! The settings drawer used to live inside [`crate::ui::app::App`];
//! moving it up to `Root` means this struct is the single source of
//! truth for every signal the drawer touches. Both `App` and the new
//! [`crate::ui::settings_drawer::SettingsDrawer`] component pull the
//! handles out via [`use_context::<AppSettings>()`]. Signals are
//! `Copy`, so the struct itself is cheap to clone and pass through
//! Dioxus contexts.
//!
//! Spec: this is the "full lift to Root" path agreed in the
//! Cartesia mid-session voice-change conversation
//! (`docs/cartesia-sonic-3-integration-spec.md` §6.4).

use dioxus::prelude::*;

use crate::stt::soniox::{
    SONIOX_CONTEXT_TEXT_STORAGE_KEY, SONIOX_LATENCY_MODE_COOKIE, SonioxLatencyMode,
};
use crate::ui::app::{load, load_local};
use crate::ui::secrets::{self, SecretsRefresh, SecretsStatus};

/// Bundle of settings-drawer state shared between `Root`, `App`,
/// `ConversationView`, and `SettingsDrawer`. Every field is a `Copy`
/// signal/memo handle, so the struct itself is `Copy`.
#[derive(Clone, Copy)]
pub struct AppSettings {
    /// Drawer visibility. Toggled by the gear button in `Root`.
    pub show_settings: Signal<bool>,

    // ── Secrets ────────────────────────────────────────────────────
    /// Refresh handle the drawer hands to each `SecretsKeyRow` so a
    /// set/delete re-runs the status fetch.
    pub secrets_refresh: SecretsRefresh,
    pub assemblyai_configured: Memo<bool>,
    pub soniox_configured: Memo<bool>,
    pub anthropic_configured: Memo<bool>,
    pub elevenlabs_configured: Memo<bool>,
    pub xai_configured: Memo<bool>,
    pub cartesia_configured: Memo<bool>,

    // ── Pipeline ───────────────────────────────────────────────────
    pub pipeline_tts_provider: Signal<String>,
    pub pipeline_tts_voice: Signal<String>,
    pub voice_credential: Signal<String>,
    pub pipeline_stt_provider: Signal<String>,
    pub soniox_latency_mode: Signal<String>,
    pub soniox_context_text: Signal<String>,

    // ── Session/cost ───────────────────────────────────────────────
    pub idle_minutes: Signal<u32>,
    pub show_cost_meter: Signal<bool>,

    // ── Speakers / formatting toggles ─────────────────────────────
    pub speaker1_name: Signal<String>,
    pub speaker1_source: Signal<String>,
    pub speaker2_enabled: Signal<bool>,
    pub speaker2_name: Signal<String>,
    pub speaker2_source: Signal<String>,
    pub show_labels: Signal<bool>,
    pub show_timestamps: Signal<bool>,

    // ── Reformatting (global, used by both Transcribe + Conversation) ─
    /// Registry id of the model `/format` should drive. The settings
    /// drawer populates this from `GET /models`. Stored in the new
    /// `parley_reformat_model_config_id` cookie; on first load we
    /// migrate from the legacy `parley_format_model` cookie if
    /// present. Spec `docs/global-reformat-spec.md` §3, §8.
    pub reformat_model_config_id: Signal<String>,
    /// Named credential to draw the formatter's provider key from.
    /// Defaults to `"default"`.
    pub reformat_credential: Signal<String>,
    /// Auto-format every Nth turn in Transcribe Mode.
    pub auto_format_enabled: Signal<bool>,
    /// `N` for the auto-format trigger (every Nth committed turn).
    pub format_nth: Signal<u32>,
    /// Reformat depth: number of trailing chunks the model is
    /// allowed to rewrite. `0` = full transcript.
    pub format_depth: Signal<usize>,
    /// Additional read-only context chunks before the editable
    /// window.
    pub format_context_depth: Signal<usize>,
    /// Run a full Sonnet pass when recording stops in Transcribe
    /// Mode.
    pub format_on_stop: Signal<bool>,
    /// Run `/format` over each user voice turn before submitting in
    /// Conversation Mode. Spec §5.
    pub auto_format_in_conversation: Signal<bool>,
}

impl AppSettings {
    /// Initialize every signal from cookies / local storage and wire
    /// the secrets-status memos. Must be called from inside a Dioxus
    /// component scope (uses `use_signal` / `use_memo`). Designed to
    /// run once at the top of `Root`.
    pub fn init() -> Self {
        let show_settings = use_signal(|| false);

        let (secrets_status, secrets_refresh) = secrets::use_secrets_status();
        let assemblyai_configured =
            use_memo(move || provider_configured(secrets_status, "assemblyai"));
        let soniox_configured = use_memo(move || provider_configured(secrets_status, "soniox"));
        let anthropic_configured =
            use_memo(move || provider_configured(secrets_status, "anthropic"));
        let elevenlabs_configured =
            use_memo(move || provider_configured(secrets_status, "elevenlabs"));
        let xai_configured = use_memo(move || provider_configured(secrets_status, "xai"));
        let cartesia_configured = use_memo(move || provider_configured(secrets_status, "cartesia"));

        let pipeline_stt_provider = use_signal(crate::ui::pipeline::stt_provider);
        let pipeline_tts_provider = use_signal(|| {
            load(crate::ui::pipeline::TTS_PROVIDER_KEY).unwrap_or_else(|| "elevenlabs".to_string())
        });
        let pipeline_tts_voice =
            use_signal(|| load(crate::ui::pipeline::TTS_VOICE_KEY).unwrap_or_default());
        let voice_credential = use_signal(|| "default".to_string());

        let soniox_latency_mode = use_signal(|| {
            load(SONIOX_LATENCY_MODE_COOKIE)
                .and_then(|value| {
                    SonioxLatencyMode::from_storage_value(&value)
                        .map(|mode| mode.storage_value().to_string())
                })
                .unwrap_or_else(|| SonioxLatencyMode::default().storage_value().to_string())
        });
        let soniox_context_text =
            use_signal(|| load_local(SONIOX_CONTEXT_TEXT_STORAGE_KEY).unwrap_or_default());

        let idle_minutes = use_signal(|| {
            load("parley_idle_minutes")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(5)
        });
        let show_cost_meter = use_signal(|| {
            load("parley_show_cost_meter")
                .map(|v| v != "false")
                .unwrap_or(true)
        });

        let speaker1_name =
            use_signal(|| load("parley_speaker1_name").unwrap_or_else(|| "Me".to_string()));
        let speaker1_source =
            use_signal(|| load("parley_speaker1_source").unwrap_or_else(|| "mic".to_string()));
        let speaker2_name =
            use_signal(|| load("parley_speaker2_name").unwrap_or_else(|| "Remote".to_string()));
        let speaker2_source =
            use_signal(|| load("parley_speaker2_source").unwrap_or_else(|| "system".to_string()));
        let speaker2_enabled = use_signal(|| {
            load("parley_speaker2_enabled")
                .map(|v| v == "true")
                .unwrap_or(false)
        });
        let show_labels = use_signal(|| {
            load("parley_show_labels")
                .map(|v| v == "true")
                .unwrap_or(true)
        });
        let show_timestamps = use_signal(|| {
            load("parley_show_timestamps")
                .map(|v| v == "true")
                .unwrap_or(false)
        });

        // ── Reformatting ──────────────────────────────────────────
        // Cookie migration: the new global setting lives in
        // `parley_reformat_model_config_id`. If the user has a stale
        // value from the old per-Transcribe `parley_format_model`
        // cookie, accept it as the initial pick — the settings
        // drawer translates legacy raw model names to registry ids
        // once `/models` returns. Spec §3, §8.
        let reformat_model_config_id = use_signal(|| {
            load("parley_reformat_model_config_id")
                .or_else(|| load("parley_format_model"))
                .unwrap_or_default()
        });
        let reformat_credential = use_signal(|| {
            load("parley_reformat_credential").unwrap_or_else(|| "default".to_string())
        });
        let auto_format_enabled = use_signal(|| {
            load("parley_auto_format")
                .map(|s| s != "false")
                .unwrap_or(true)
        });
        let format_nth = use_signal(|| {
            load("parley_format_nth")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(3)
        });
        let format_depth = use_signal(|| {
            load("parley_format_depth")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(2)
        });
        let format_context_depth = use_signal(|| {
            load("parley_format_context_depth")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(1)
        });
        let format_on_stop = use_signal(|| {
            load("parley_format_on_stop")
                .map(|s| s == "true")
                .unwrap_or(true)
        });
        let auto_format_in_conversation = use_signal(|| {
            load("parley_auto_format_in_conversation")
                .map(|s| s != "false")
                .unwrap_or(true)
        });

        // Persist pipeline-pick changes back to cookies. Mirrors the
        // effects that previously lived in `App`. `use_reactive!` only
        // fires when the captured signal changes, so these don't run
        // on every render.
        use_effect(use_reactive!(|pipeline_stt_provider| {
            crate::ui::app::save(
                crate::ui::pipeline::STT_PROVIDER_KEY,
                &pipeline_stt_provider(),
            );
        }));
        use_effect(use_reactive!(|pipeline_tts_provider| {
            crate::ui::app::save(
                crate::ui::pipeline::TTS_PROVIDER_KEY,
                &pipeline_tts_provider(),
            );
        }));
        use_effect(use_reactive!(|pipeline_tts_voice| {
            crate::ui::app::save(crate::ui::pipeline::TTS_VOICE_KEY, &pipeline_tts_voice());
        }));

        // Persist reformat settings on every change. Mirrors the
        // pipeline-pick effects above so the drawer doesn't have to
        // sprinkle `save()` calls inline.
        use_effect(use_reactive!(|reformat_model_config_id| {
            let val = reformat_model_config_id();
            if !val.is_empty() {
                crate::ui::app::save("parley_reformat_model_config_id", &val);
            }
        }));
        use_effect(use_reactive!(|reformat_credential| {
            crate::ui::app::save("parley_reformat_credential", &reformat_credential());
        }));
        use_effect(use_reactive!(|auto_format_enabled| {
            crate::ui::app::save(
                "parley_auto_format",
                if auto_format_enabled() {
                    "true"
                } else {
                    "false"
                },
            );
        }));
        use_effect(use_reactive!(|format_nth| {
            crate::ui::app::save("parley_format_nth", &format_nth().to_string());
        }));
        use_effect(use_reactive!(|format_depth| {
            crate::ui::app::save("parley_format_depth", &format_depth().to_string());
        }));
        use_effect(use_reactive!(|format_context_depth| {
            crate::ui::app::save(
                "parley_format_context_depth",
                &format_context_depth().to_string(),
            );
        }));
        use_effect(use_reactive!(|format_on_stop| {
            crate::ui::app::save(
                "parley_format_on_stop",
                if format_on_stop() { "true" } else { "false" },
            );
        }));
        use_effect(use_reactive!(|auto_format_in_conversation| {
            crate::ui::app::save(
                "parley_auto_format_in_conversation",
                if auto_format_in_conversation() {
                    "true"
                } else {
                    "false"
                },
            );
        }));

        AppSettings {
            show_settings,
            secrets_refresh,
            assemblyai_configured,
            soniox_configured,
            anthropic_configured,
            elevenlabs_configured,
            xai_configured,
            cartesia_configured,
            pipeline_tts_provider,
            pipeline_tts_voice,
            voice_credential,
            pipeline_stt_provider,
            soniox_latency_mode,
            soniox_context_text,
            idle_minutes,
            show_cost_meter,
            speaker1_name,
            speaker1_source,
            speaker2_enabled,
            speaker2_name,
            speaker2_source,
            show_labels,
            show_timestamps,
            reformat_model_config_id,
            reformat_credential,
            auto_format_enabled,
            format_nth,
            format_depth,
            format_context_depth,
            format_on_stop,
            auto_format_in_conversation,
        }
    }
}

fn provider_configured(
    secrets_status: Resource<Result<SecretsStatus, String>>,
    provider: &str,
) -> bool {
    matches!(
        &*secrets_status.read_unchecked(),
        Some(Ok(s)) if s
            .provider(provider)
            .and_then(|p| p.credential("default"))
            .map(|c| c.configured)
            .unwrap_or(false),
    )
}
