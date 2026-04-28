//! Format-aware browser-side TTS playback dispatch.
//!
//! Conversation Mode no longer assumes MP3 — Cartesia Sonic-3 emits
//! raw `pcm_s16le` over WebSocket and the proxy preserves it
//! end-to-end. The browser learns which container the live SSE
//! stream is carrying from a leading `format` event (see
//! `proxy::conversation_api::format_event`), and constructs the
//! matching sink:
//!
//! - `audio/mpeg`                       → [`MediaSourcePlayer`]
//! - `audio/pcm-s16le-44100-mono`       → [`PcmPlayer`]
//! - anything else                      → MP3 fallback (for
//!   resilience: a future MP3 variant added on the proxy side
//!   shouldn't soft-break old browsers).
//!
//! Spec: `docs/cartesia-sonic-3-integration-spec.md` §6.4.

use wasm_bindgen::prelude::JsValue;

use super::media_player::MediaSourcePlayer;
use super::pcm_player::PcmPlayer;

/// One of the two concrete audio sinks the browser maintains for a
/// live TTS stream. Cheap to clone — both variants delegate to an
/// `Rc<RefCell<...>>` internally.
#[derive(Clone)]
pub enum TtsAudioSink {
    /// Progressive MP3 backed by `MediaSource` + `<audio>`.
    Mp3(MediaSourcePlayer),
    /// Raw PCM backed by Web Audio (`AudioContext` +
    /// `AudioBufferSourceNode` queue).
    Pcm(PcmPlayer),
}

impl TtsAudioSink {
    /// Construct an MP3 sink. Mirrors `MediaSourcePlayer::new`.
    pub fn mp3() -> Result<Self, JsValue> {
        Ok(Self::Mp3(MediaSourcePlayer::new()?))
    }

    /// Construct a PCM sink for Cartesia's pinned 44.1 kHz mono
    /// `pcm_s16le` shape.
    pub fn pcm_default() -> Self {
        Self::Pcm(PcmPlayer::new_default())
    }

    /// Construct a sink from the proxy's `format` SSE frame
    /// payload. Unknown MIMEs fall back to MP3 — see the module
    /// header for the rationale.
    pub fn from_mime(mime: &str) -> Result<Self, JsValue> {
        match mime {
            "audio/pcm-s16le-44100-mono" => Ok(Self::pcm_default()),
            // `audio/mpeg` (and anything else, defensively).
            _ => Self::mp3(),
        }
    }

    /// Append an audio chunk in the sink's native container shape.
    /// MP3 callers pass raw MP3 frame bytes; PCM callers pass
    /// interleaved `pcm_s16le` samples.
    pub fn append(&self, bytes: &[u8]) -> Result<(), JsValue> {
        match self {
            Self::Mp3(p) => p.append(bytes),
            Self::Pcm(p) => p.append(bytes),
        }
    }

    /// Mark end-of-stream. Both sinks finish draining any buffered
    /// audio and then surface the `ended` event.
    pub fn end(&self) -> Result<(), JsValue> {
        match self {
            Self::Mp3(p) => p.end(),
            Self::Pcm(p) => p.end(),
        }
    }

    /// Pause playback at the current cursor.
    pub fn pause(&self) {
        match self {
            Self::Mp3(p) => p.pause(),
            Self::Pcm(p) => p.pause(),
        }
    }

    /// Resume playback. Returns `Err` only on the MP3 path when
    /// the browser rejects the play promise (typically because no
    /// user gesture has been observed yet); the PCM path resumes
    /// the suspended `AudioContext` synchronously.
    pub fn play(&self) -> Result<(), JsValue> {
        match self {
            Self::Mp3(p) => p.play(),
            Self::Pcm(p) => p.play(),
        }
    }

    /// Halt playback, detach the underlying browser objects, and
    /// release any allocated buffers / contexts. Idempotent.
    pub fn stop(&self) {
        match self {
            Self::Mp3(p) => p.stop(),
            Self::Pcm(p) => p.stop(),
        }
    }

    /// Subscribe to playback-finished events. Both sinks fire the
    /// callback after the last buffered audio plays out following
    /// an [`Self::end`] call.
    pub fn on_ended(&self, cb: Box<dyn Fn()>) {
        match self {
            Self::Mp3(p) => p.on_ended(cb),
            Self::Pcm(p) => p.on_ended(cb),
        }
    }
}
