//! STT (speech-to-text) canonical types shared between the WASM frontend
//! and the native proxy. These are *data shapes* — the `SttProvider`
//! trait that consumes them lives in `parley-proxy` for the same reason
//! `LlmProvider` does (async-trait + HTTP machinery don't belong in the
//! WASM bundle).
//!
//! Spec references: `docs/xai-speech-integration-spec.md` §6.2 (trait
//! surface), §6.6 (orchestrator routing).

use serde::{Deserialize, Serialize};

/// Audio container/codec shape an [`SttRequest`] or streaming session
/// carries. Only the variants we actually feed providers today are
/// listed; add more as needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SttAudioFormat {
    /// 16-bit signed PCM, little-endian, mono. The `sample_rate_hz`
    /// must match what the browser's `AudioContext` is producing.
    Pcm16Le {
        /// Sampling rate of the PCM stream, in Hz (e.g., 16000).
        sample_rate_hz: u32,
    },
    /// RIFF/WAV container. Sample rate and bit depth are read from the
    /// header by the provider.
    Wav,
    /// MPEG-1 Audio Layer III.
    Mp3,
    /// Opus in an Ogg container.
    Opus,
    /// Free Lossless Audio Codec.
    Flac,
}

/// One file/batch transcription request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SttRequest {
    /// Raw audio bytes. The provider is expected to decode these
    /// according to `format`.
    pub audio: Vec<u8>,
    /// Container/codec of the `audio` bytes.
    pub format: SttAudioFormat,
    /// BCP-47 language hint. `None` requests auto-detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Whether to attribute utterances to distinct speakers. The
    /// default (`true`) matches OQ-06's resolution — diarization stays
    /// on to satisfy the existing speaker-separation UX.
    #[serde(default = "default_diarize")]
    pub diarize: bool,
}

fn default_diarize() -> bool {
    true
}

/// Initial configuration a streaming STT session receives before any
/// audio frames arrive. The proxy-side `SttProvider::stream` consumes
/// one of these and returns a pair of channels (sink for audio, source
/// for events).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SttStreamConfig {
    /// Audio format the client will push. Streaming is PCM-only in v1
    /// (no container framing), so the variant is typically
    /// `Pcm16Le { sample_rate_hz }`.
    pub format: SttAudioFormat,
    /// BCP-47 language hint. `None` requests auto-detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Whether to attribute utterances to distinct speakers.
    #[serde(default = "default_diarize")]
    pub diarize: bool,
}

/// Final transcript from a batch call. Also the accumulated final state
/// of a streaming session, materialized by the orchestrator on
/// `TranscriptEvent::Done`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    /// Whole-transcript concatenated text.
    pub text: String,
    /// Per-utterance segments when diarization or timing is present.
    /// Empty when the provider returns only whole-text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<TranscriptSegment>,
    /// BCP-47 language code the provider detected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Total audio duration in seconds. Used as the billable basis for
    /// `SttProvider::cost(seconds, streaming)`.
    pub duration_seconds: f64,
}

/// One utterance-shaped segment within a [`Transcript`]. Timestamps
/// are seconds from the start of the input audio.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptSegment {
    /// Text of this segment.
    pub text: String,
    /// Start offset within the audio, in seconds.
    pub start_seconds: f64,
    /// End offset within the audio, in seconds.
    pub end_seconds: f64,
    /// Speaker identifier (e.g. `"A"`, `"B"`) when diarized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
}

/// One incremental event from a streaming STT session.
///
/// `Partial` hypotheses may be revised or wholly replaced; `Final`
/// utterances are stable once emitted. A single `Done` terminates the
/// stream and carries the session's billable duration so the
/// orchestrator can finalize STT cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptEvent {
    /// Interim transcription hypothesis. May be replaced by a later
    /// `Partial` or superseded by a `Final`.
    Partial {
        /// Running text hypothesis.
        text: String,
    },
    /// Finalized utterance — stable from emission onward.
    Final {
        /// Finalized text.
        text: String,
        /// Speaker id when diarized.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        speaker: Option<String>,
        /// Utterance start, seconds from session start.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start_seconds: Option<f64>,
        /// Utterance end, seconds from session start.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end_seconds: Option<f64>,
    },
    /// End-of-stream marker. `duration_seconds` is the billable audio
    /// duration (streaming-rate basis).
    Done {
        /// Billable session duration in seconds.
        duration_seconds: f64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_format_pcm_round_trip() {
        let f = SttAudioFormat::Pcm16Le {
            sample_rate_hz: 16000,
        };
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains("\"kind\":\"pcm16_le\""));
        assert!(json.contains("\"sample_rate_hz\":16000"));
        let parsed: SttAudioFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, f);
    }

    #[test]
    fn diarize_defaults_to_true_on_deserialize() {
        // Field omitted from the wire entirely.
        let json = r#"{"audio":[1,2,3],"format":{"kind":"wav"}}"#;
        let req: SttRequest = serde_json::from_str(json).unwrap();
        assert!(req.diarize);
        assert!(req.language.is_none());
    }

    #[test]
    fn transcript_omits_empty_segments_and_none_language() {
        let t = Transcript {
            text: "hi".into(),
            segments: vec![],
            language: None,
            duration_seconds: 1.5,
        };
        let json = serde_json::to_string(&t).unwrap();
        assert!(!json.contains("segments"));
        assert!(!json.contains("language"));
        assert!(json.contains("\"duration_seconds\":1.5"));
    }

    #[test]
    fn transcript_event_partial_round_trip() {
        let e = TranscriptEvent::Partial {
            text: "hello wor".into(),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"kind\":\"partial\""));
        assert_eq!(serde_json::from_str::<TranscriptEvent>(&json).unwrap(), e);
    }

    #[test]
    fn transcript_event_final_with_speaker_round_trip() {
        let e = TranscriptEvent::Final {
            text: "hello world".into(),
            speaker: Some("A".into()),
            start_seconds: Some(0.4),
            end_seconds: Some(1.8),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"kind\":\"final\""));
        assert!(json.contains("\"speaker\":\"A\""));
        assert_eq!(serde_json::from_str::<TranscriptEvent>(&json).unwrap(), e);
    }

    #[test]
    fn transcript_event_done_round_trip() {
        let e = TranscriptEvent::Done {
            duration_seconds: 42.5,
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"kind\":\"done\""));
        assert!(json.contains("\"duration_seconds\":42.5"));
        assert_eq!(serde_json::from_str::<TranscriptEvent>(&json).unwrap(), e);
    }

    #[test]
    fn stream_config_round_trips_with_pcm() {
        let c = SttStreamConfig {
            format: SttAudioFormat::Pcm16Le {
                sample_rate_hz: 16000,
            },
            language: Some("en".into()),
            diarize: true,
        };
        let json = serde_json::to_string(&c).unwrap();
        let parsed: SttStreamConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, c);
    }
}
