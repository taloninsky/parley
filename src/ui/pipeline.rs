//! Single source of truth for the user's selected pipeline (STT, TTS,
//! voice). Both the in-place transcription view (`app.rs`) and the
//! conversation view (`conversation.rs`) read these keys at bootstrap
//! time so a change in Settings → Pipeline takes effect everywhere.
//!
//! Persistence rides on the same cookie-backed `load`/`save` helpers
//! the rest of `app.rs` already uses (cookies, not localStorage —
//! shared across all `localhost:*` ports during dev).
//!
//! ## Wiring status (today)
//!
//! - `tts_provider` / `tts_voice_id` — fully effectful. Threaded into
//!   `POST /conversation/init` so the proxy picks the matching
//!   `TtsProvider` impl.
//! - `stt_provider` — fully effectful for Conversation Mode (the
//!   `use_voice_input` hook reads this and dispatches between
//!   `AssemblyAiSession` and `XaiProxySession`). The standalone
//!   transcription view in `app.rs` is still AssemblyAI-only — its
//!   dual-session formatter pipeline hasn't been ported yet.

use crate::ui::app;

/// Cookie key for the selected STT provider. Values: `assemblyai`,
/// `xai`. Anything else is treated as the default (AssemblyAI today).
pub const STT_PROVIDER_KEY: &str = "parley_pipeline_stt";

/// Cookie key for the selected TTS provider. Values: `elevenlabs`,
/// `xai`, `off`. Missing key → ElevenLabs (back-compat with sessions
/// that pre-date the picker).
pub const TTS_PROVIDER_KEY: &str = "parley_pipeline_tts";

/// Cookie key for the selected TTS voice id (provider-specific).
/// Empty / missing → proxy applies the provider default.
pub const TTS_VOICE_KEY: &str = "parley_pipeline_tts_voice";

/// Read the current STT provider id, defaulting to `assemblyai` when
/// the user has never touched the picker. `use_voice_input` reads this
/// at start-of-session to choose between AssemblyAI's direct-token WS
/// and the proxy-bridged xAI WS.
pub fn stt_provider() -> String {
    app::load(STT_PROVIDER_KEY).unwrap_or_else(|| "assemblyai".to_string())
}

/// Read the current TTS provider id, defaulting to `elevenlabs`.
pub fn tts_provider() -> String {
    app::load(TTS_PROVIDER_KEY).unwrap_or_else(|| "elevenlabs".to_string())
}

/// Read the current TTS voice id. Empty string when the user has not
/// chosen one — callers should treat that as "use provider default".
pub fn tts_voice_id() -> String {
    app::load(TTS_VOICE_KEY).unwrap_or_default()
}
