//! Streaming text-to-speech providers.
//!
//! Mirrors the `llm` module: a thin trait + per-provider impl, with
//! the orchestrator owning the dispatch loop. The chunker that
//! decides what text to send lives in `parley-core::tts` so it can
//! be unit-tested across the workspace.
//!
//! Spec: `docs/conversation-voice-slice-spec.md` §4.2 and §6.

pub mod cache;
pub mod cartesia;
pub mod elevenlabs;
pub mod hub;
pub mod silence;
pub mod xai;

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use parley_core::chat::Cost;
use parley_core::tts::{ChunkPolicy, VoiceDescriptor};
use thiserror::Error;
use tokio::sync::mpsc;

// Re-exports kept here so the orchestrator and HTTP layer can
// reference `crate::tts::Foo` instead of going through the
// submodule path. `TtsCacheReader` and `ElevenLabsTts` aren't yet
// referenced through the re-export (only by absolute path / from
// startup wiring landing in Task 8); allow the dead-code lint until
// then.
#[allow(unused_imports)]
pub use cache::{FsTtsCache, TtsCacheReader, TtsCacheWriter};
#[allow(unused_imports)]
pub use cartesia::CartesiaTts;
#[allow(unused_imports)]
pub use elevenlabs::ElevenLabsTts;
pub use hub::{TtsBroadcastFrame, TtsBroadcaster, TtsHub};
#[allow(unused_imports)]
pub use xai::XaiTts;

/// One synthesis request handed to a [`TtsProvider`]. The
/// orchestrator builds these from [`parley_core::tts::SentenceChunk`]
/// values; we do not pass the chunk type through the trait to keep
/// the provider crate-agnostic about chunking policy.
#[derive(Debug, Clone)]
pub struct TtsRequest {
    /// Voice identifier as understood by the provider. For ElevenLabs
    /// this is the voice id (e.g. `"c6SfcYrb2t09NHXiT80T"` for
    /// "Jarnathan").
    pub voice_id: String,
    /// Text to synthesize. Must be non-empty; the provider may reject
    /// or truncate empty strings.
    pub text: String,
}

/// One frame of a streaming TTS response. Audio frames carry raw
/// container bytes (MP3 for ElevenLabs); the terminal `Done` frame
/// reports billing-grade character counts so the orchestrator can
/// finalize cost.
#[derive(Debug, Clone)]
pub enum TtsChunk {
    /// Raw audio bytes ready for the cache file and the live SSE
    /// fan-out. Container framing (MP3) is preserved verbatim.
    Audio(Vec<u8>),
    /// End-of-stream marker. `characters` is the *billable* count
    /// (typically the input text length); the orchestrator uses it
    /// to compute cost via [`TtsProvider::cost`].
    Done {
        /// Billable character count for this request.
        characters: u32,
    },
}

/// Errors a [`TtsProvider`] can surface. Distinct from `LlmError`
/// because TTS failures should not roll back the LLM turn — the
/// orchestrator will catch these and emit a non-fatal `Failed` event
/// while still appending the AI turn to the session.
#[derive(Debug, Error)]
pub enum TtsError {
    /// Network or transport failure talking to the provider.
    #[error("tts transport error: {0}")]
    Transport(String),
    /// Provider returned a non-success HTTP status. `status` is the
    /// HTTP code; `body` is the response body (truncated by the
    /// caller if large).
    #[error("tts http error: status={status}, body={body}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Response body as a string (best-effort decode).
        body: String,
    },
    /// Provider response could not be parsed (e.g. invalid event
    /// frame, unexpected EOF mid-chunk).
    #[error("tts protocol error: {0}")]
    Protocol(String),
    /// Feature not supported by this provider (e.g. requesting a
    /// voices catalog from a provider that doesn't publish one).
    #[error("tts unsupported: {0}")]
    Unsupported(String),
    /// Catch-all for provider-specific failures that don't fit the
    /// above buckets.
    #[error("tts error: {0}")]
    Other(String),
}

/// Stream of audio chunks plus a terminal `Done`. Modeled exactly
/// like `LlmProvider::stream_chat` so the orchestrator's loop shape
/// is symmetric.
pub type TtsStream = Pin<Box<dyn Stream<Item = Result<TtsChunk, TtsError>> + Send>>;

/// One turn-level streaming TTS request. Unlike [`TtsRequest`], this
/// does not carry all text up front; callers feed text deltas through
/// [`TurnTextStream::text_tx`] while audio arrives concurrently.
#[derive(Debug, Clone)]
pub struct TtsTurnStreamRequest {
    /// Voice identifier as understood by the provider.
    pub voice_id: String,
}

/// Text input sent to a turn-level streaming TTS session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TtsTextFrame {
    /// Additional synthesizable text. Providers may buffer or forward
    /// it immediately depending on their native protocol.
    Delta(String),
    /// No more text will arrive for this turn.
    Done,
}

/// Full-duplex turn-level TTS session. The caller sends text frames
/// into `text_tx`; the provider emits audio frames on `audio`.
pub struct TurnTextStream {
    /// Text input sink. Dropping this sender is equivalent to sending
    /// [`TtsTextFrame::Done`].
    pub text_tx: mpsc::Sender<TtsTextFrame>,
    /// Provider audio output stream.
    pub audio: TtsStream,
}

/// Audio container/codec shape a [`TtsProvider`] returns. The enum
/// exists so the `SilenceSplicer` can pick matching zero-energy
/// bytes, the cache can record the format alongside the bytes on
/// disk, and the live SSE pipeline can advertise the right MIME to
/// the browser.
///
/// Spec: `docs/paragraph-tts-chunking-spec.md` §3.2 and
/// `docs/cartesia-sonic-3-integration-spec.md` §6.4.
//
// `non_camel_case_types` is silenced for the whole enum because the
// variant names embed sample rate / bit rate / channel layout, which
// reads more naturally with separators (`Pcm_S16LE_44100_Mono` vs
// the lint-preferred `PcmS16Le44100Mono`).
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    /// 44.1 kHz, 128 kbps, joint-stereo MPEG-1 Layer III (CBR).
    /// ElevenLabs and xAI default; matches the embedded silence
    /// frame in [`silence::SILENCE_FRAME_44100_128_STEREO`].
    Mp3_44100_128,
    /// 16-bit signed little-endian linear PCM, mono, 44.1 kHz. The
    /// Cartesia WebSocket endpoint emits this (it doesn't accept
    /// `container: "mp3"` over WS at all). 88 200 bytes/second per
    /// stream.
    Pcm_S16LE_44100_Mono,
}

impl AudioFormat {
    /// MIME / content-type the proxy advertises to the browser for
    /// this format. PCM has no native browser MIME, so we wrap it in
    /// a streaming WAV header at the replay endpoint and advertise
    /// `audio/wav`. The live SSE format frame uses the per-format
    /// canonical string from [`Self::sse_mime`].
    pub fn replay_mime(&self) -> &'static str {
        match self {
            Self::Mp3_44100_128 => "audio/mpeg",
            Self::Pcm_S16LE_44100_Mono => "audio/wav",
        }
    }

    /// MIME-style identifier the live SSE `event: format` frame
    /// emits to the browser. PCM uses a Parley-specific string
    /// (`audio/pcm-s16le-44100-mono`) so the browser dispatches to
    /// the Web Audio sink rather than handing it to MediaSource.
    pub fn sse_mime(&self) -> &'static str {
        match self {
            Self::Mp3_44100_128 => "audio/mpeg",
            Self::Pcm_S16LE_44100_Mono => "audio/pcm-s16le-44100-mono",
        }
    }

    /// Lower-case ASCII tag used in cache filenames and the
    /// `/api/tts/synthesize` JSON response (`audio_format`). Must be
    /// stable; existing callers persist this string.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Mp3_44100_128 => "mp3_44100_128",
            Self::Pcm_S16LE_44100_Mono => "pcm_s16le_44100_mono",
        }
    }

    /// File extension (without the leading dot) for cache files.
    /// The cache writer composes `{turn_id}.{ext}` and writes a
    /// sidecar `{turn_id}.fmt` carrying [`Self::tag`] so the reader
    /// can recover the format on disk.
    pub fn cache_extension(&self) -> &'static str {
        match self {
            Self::Mp3_44100_128 => "mp3",
            Self::Pcm_S16LE_44100_Mono => "pcm",
        }
    }

    /// Sample rate in Hz. Used by the silence splicer to compute
    /// PCM zero-byte counts and by the live SSE format frame to
    /// inform the browser's `AudioContext`.
    pub fn sample_rate(&self) -> u32 {
        match self {
            Self::Mp3_44100_128 | Self::Pcm_S16LE_44100_Mono => 44_100,
        }
    }

    /// Channel count.
    pub fn channels(&self) -> u16 {
        match self {
            Self::Mp3_44100_128 => 2,
            Self::Pcm_S16LE_44100_Mono => 1,
        }
    }

    /// Decoded bytes per sample per channel. Only meaningful for raw
    /// PCM containers; for MP3 the splicer ignores this and uses the
    /// pre-baked silence frame directly.
    pub fn bytes_per_sample(&self) -> u16 {
        match self {
            Self::Mp3_44100_128 => 0,
            Self::Pcm_S16LE_44100_Mono => 2,
        }
    }

    /// Round-trip the on-disk tag back into an [`AudioFormat`]. Used
    /// by the cache reader to recover the format from a sidecar file.
    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "mp3_44100_128" => Some(Self::Mp3_44100_128),
            "pcm_s16le_44100_mono" => Some(Self::Pcm_S16LE_44100_Mono),
            _ => None,
        }
    }

    /// Build a 44-byte RIFF/WAVE header for a `data_size`-byte PCM
    /// payload at this format, or `None` for formats (MP3) that
    /// already self-describe their container. Pass
    /// [`STREAMING_WAV_DATA_SIZE`] (0xFFFFFFFF) when the total size
    /// is unknown — most browsers tolerate the sentinel and stream
    /// the body indefinitely. Spec:
    /// `docs/cartesia-sonic-3-integration-spec.md` §6.4.
    pub fn wav_header(&self, data_size: u32) -> Option<Vec<u8>> {
        match self {
            Self::Mp3_44100_128 => None,
            Self::Pcm_S16LE_44100_Mono => Some(build_wav_header(self, data_size)),
        }
    }
}

/// Sentinel `data_size` value for live (length-unknown) streaming
/// WAV. Browsers (Chrome, Firefox, Safari) tolerate this and keep
/// reading body bytes until EOF. See spec §6.4.
pub const STREAMING_WAV_DATA_SIZE: u32 = 0xFFFF_FFFF;

/// Construct the canonical 44-byte RIFF/WAVE PCM header. Pulled out
/// so unit tests can exercise the byte layout directly without going
/// through the `AudioFormat` enum.
fn build_wav_header(format: &AudioFormat, data_size: u32) -> Vec<u8> {
    let sample_rate = format.sample_rate();
    let channels = format.channels();
    let bytes_per_sample = format.bytes_per_sample();
    let bits_per_sample = (bytes_per_sample as u16) * 8;
    let block_align = channels * bytes_per_sample;
    let byte_rate = sample_rate * (block_align as u32);
    // RIFF size is the file size minus the leading `RIFF<size>`
    // header (8 bytes). With the 36-byte format chunk plus the
    // `data` chunk header (8 bytes) plus payload, that's
    // `36 + data_size`. When `data_size` is the streaming
    // sentinel, we propagate it as the riff size too — browsers
    // treat both as "indefinite".
    let riff_size = if data_size == STREAMING_WAV_DATA_SIZE {
        STREAMING_WAV_DATA_SIZE
    } else {
        data_size.saturating_add(36)
    };
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&riff_size.to_le_bytes());
    h.extend_from_slice(b"WAVE");
    // fmt chunk
    h.extend_from_slice(b"fmt ");
    h.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size (PCM)
    h.extend_from_slice(&1u16.to_le_bytes()); // audio format = PCM
    h.extend_from_slice(&channels.to_le_bytes());
    h.extend_from_slice(&sample_rate.to_le_bytes());
    h.extend_from_slice(&byte_rate.to_le_bytes());
    h.extend_from_slice(&block_align.to_le_bytes());
    h.extend_from_slice(&bits_per_sample.to_le_bytes());
    // data chunk
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data_size.to_le_bytes());
    debug_assert_eq!(h.len(), 44);
    h
}

#[cfg(test)]
mod wav_header_tests {
    use super::*;

    #[test]
    fn mp3_returns_no_wav_header() {
        assert!(AudioFormat::Mp3_44100_128.wav_header(1234).is_none());
    }

    #[test]
    fn pcm_header_is_44_bytes_with_correct_riff_and_data_sizes() {
        let h = AudioFormat::Pcm_S16LE_44100_Mono.wav_header(1000).unwrap();
        assert_eq!(h.len(), 44);
        assert_eq!(&h[0..4], b"RIFF");
        // riff_size = 1000 + 36 = 1036
        assert_eq!(u32::from_le_bytes(h[4..8].try_into().unwrap()), 1036);
        assert_eq!(&h[8..12], b"WAVE");
        assert_eq!(&h[12..16], b"fmt ");
        // fmt_size = 16
        assert_eq!(u32::from_le_bytes(h[16..20].try_into().unwrap()), 16);
        // audio_format = 1 (PCM)
        assert_eq!(u16::from_le_bytes(h[20..22].try_into().unwrap()), 1);
        // channels = 1 (mono)
        assert_eq!(u16::from_le_bytes(h[22..24].try_into().unwrap()), 1);
        // sample_rate = 44100
        assert_eq!(u32::from_le_bytes(h[24..28].try_into().unwrap()), 44_100);
        // byte_rate = 44100 * 1 * 2 = 88200
        assert_eq!(u32::from_le_bytes(h[28..32].try_into().unwrap()), 88_200);
        // block_align = 1 * 2 = 2
        assert_eq!(u16::from_le_bytes(h[32..34].try_into().unwrap()), 2);
        // bits_per_sample = 16
        assert_eq!(u16::from_le_bytes(h[34..36].try_into().unwrap()), 16);
        assert_eq!(&h[36..40], b"data");
        // data_size = 1000
        assert_eq!(u32::from_le_bytes(h[40..44].try_into().unwrap()), 1000);
    }

    #[test]
    fn pcm_streaming_sentinel_propagates_to_riff_size() {
        let h = AudioFormat::Pcm_S16LE_44100_Mono
            .wav_header(STREAMING_WAV_DATA_SIZE)
            .unwrap();
        assert_eq!(
            u32::from_le_bytes(h[4..8].try_into().unwrap()),
            STREAMING_WAV_DATA_SIZE
        );
        assert_eq!(
            u32::from_le_bytes(h[40..44].try_into().unwrap()),
            STREAMING_WAV_DATA_SIZE
        );
    }
}

/// Provider-specific opaque continuation handle carried inside a
/// [`SynthesisContext`]. Each provider that wants cross-chunk
/// continuity defines its own variant.
#[derive(Debug, Clone)]
pub enum ProviderContinuationState {
    /// ElevenLabs HTTP `request-id` header from the prior chunk's
    /// response. Used to populate `previous_request_ids` on the next
    /// request when the v2 stitching model is in play. The default
    /// adapter (v3) ignores this.
    ElevenLabsRequestId(String),
    /// Cartesia per-turn `context_id`. The first chunk in a turn
    /// allocates a fresh UUID; subsequent chunks pass the same id with
    /// `continue: true` so Sonic-3 produces prosody-continuous audio
    /// across chunk boundaries. Spec:
    /// `docs/cartesia-sonic-3-integration-spec.md` §7.1.
    Cartesia(String),
}

/// Cross-chunk continuity hints passed into [`TtsProvider::synthesize`].
/// All fields are advisory — providers that don't support a given
/// hint simply ignore it.
///
/// Spec: `docs/paragraph-tts-chunking-spec.md` §3.2.
#[derive(Debug, Clone, Default)]
pub struct SynthesisContext {
    /// Text of the immediately prior chunk in the same turn, if any.
    /// May be used as a `previous_text` hint for prosody.
    pub previous_text: Option<String>,
    /// Hint at the next chunk's text when the orchestrator already
    /// has it buffered (rare in pure streaming).
    pub next_text_hint: Option<String>,
    /// Zero-based index of this chunk within the turn.
    pub chunk_index: u32,
    /// `true` when this is the last chunk for the turn.
    pub final_for_turn: bool,
    /// Provider-specific opaque continuation state from the prior
    /// chunk's response.
    pub provider_state: Option<ProviderContinuationState>,
}

/// Streaming TTS provider. Implementations are constructed once at
/// proxy startup with their API key already resolved.
#[async_trait]
pub trait TtsProvider: Send + Sync {
    /// Stable provider id (e.g. `"elevenlabs"`). Used for logging
    /// and registry lookups; not the same shape as `LlmProvider::id`
    /// but plays the same role.
    fn id(&self) -> &'static str;

    /// Synthesize `request.text` in the requested voice. Returns a
    /// stream of audio chunks terminated by a single `Done`.
    ///
    /// `ctx` carries cross-chunk continuity hints. Providers that
    /// don't use a given hint must ignore it without error.
    async fn synthesize(
        &self,
        request: TtsRequest,
        ctx: SynthesisContext,
    ) -> Result<TtsStream, TtsError>;

    /// Whether this provider supports one continuous TTS session for
    /// a whole assistant turn. Providers that return `true` should
    /// implement [`Self::open_turn_text_stream`].
    fn supports_turn_text_stream(&self) -> bool {
        false
    }

    /// Open a full-duplex turn-level text stream. This is the native
    /// shape for providers such as xAI's TTS WebSocket, where Parley
    /// can send `text.delta` frames while audio frames arrive from the
    /// same session. REST-style providers keep the default
    /// `Unsupported` implementation and continue using paragraph
    /// chunking through [`Self::synthesize`].
    async fn open_turn_text_stream(
        &self,
        _request: TtsTurnStreamRequest,
    ) -> Result<TurnTextStream, TtsError> {
        Err(TtsError::Unsupported(
            "turn-level text streaming is not supported by this provider".into(),
        ))
    }

    /// Audio format produced by [`Self::synthesize`]. Used by the
    /// `SilenceSplicer` to pick a matching silence frame.
    fn output_format(&self) -> AudioFormat;

    /// Provider-specific tuning for the model's configured chunking
    /// policy. Providers with no continuation channel can prefer
    /// larger paragraph-shaped requests; providers with strong native
    /// continuation can keep lower-latency defaults.
    fn tune_chunk_policy(&self, policy: ChunkPolicy) -> ChunkPolicy {
        policy
    }

    /// Provider-specific instruction to prepend to the LLM system
    /// prompt when expression annotations are enabled for the active
    /// persona. Each TTS model must advertise only the tags/spans its
    /// own translator can render safely.
    ///
    /// Returning `None` means the orchestrator should not ask the LLM
    /// to emit expression markup for this provider.
    fn expression_tag_instruction(&self) -> Option<String> {
        None
    }

    /// Translate Parley's neutral expression vocabulary
    /// (`{warm}`, `{laugh}`, `{pause:short}`, …; see
    /// [`parley_core::expression`]) into this provider's native tag
    /// syntax. Called once per chunk just before [`Self::synthesize`].
    ///
    /// The default implementation strips every neutral tag. Providers
    /// that do support tags override and emit native equivalents.
    /// Pure / synchronous: no I/O on the hot path.
    fn translate_expression_tags(&self, text: &str) -> String {
        parley_core::expression::strip_neutral_tags(text)
    }

    /// Compute the USD cost for `characters` synthesized in this
    /// provider's pricing tier. Pure function — no I/O.
    fn cost(&self, characters: u32) -> Cost;

    /// List available voices for the provider's voice picker UI.
    ///
    /// Providers that serve a catalog (xAI: `GET /v1/tts/voices`,
    /// ElevenLabs: `GET /v1/voices`) fetch it and map into the shared
    /// [`VoiceDescriptor`] shape. Providers with a fixed, documented
    /// voice list can return it synthetically. Providers that don't
    /// expose voice selection at all return
    /// [`TtsError::Unsupported`] — the default impl does that so
    /// existing test providers keep compiling.
    ///
    /// Implementations SHOULD cache the upstream response (see spec
    /// §5.6: 24-hour TTL is the proxy contract) so that the voice
    /// picker doesn't re-hit the upstream on every render. The cache
    /// is an implementation detail, not part of the trait.
    async fn voices(&self) -> Result<Vec<VoiceDescriptor>, TtsError> {
        Err(TtsError::Unsupported(
            "voices catalog not implemented".into(),
        ))
    }
}
