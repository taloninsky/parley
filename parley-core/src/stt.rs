//! Shared speech-to-text data shapes and stream normalization.
//!
//! The batch/stream transcript types are the canonical wire contract between
//! the WASM frontend and the native proxy. The token-stream types support
//! Soniox's token-native realtime API and keep its normalization logic in the
//! WASM-safe core crate.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::word_graph::{SttWord, WordGraph};

/// Audio container/codec shape an [`SttRequest`] or streaming session carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SttAudioFormat {
    /// 16-bit signed PCM, little-endian, mono.
    Pcm16Le {
        /// Sampling rate of the PCM stream, in Hz.
        sample_rate_hz: u32,
    },
    /// RIFF/WAV container.
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
    /// Raw audio bytes. The provider decodes these according to [`Self::format`].
    pub audio: Vec<u8>,
    /// Container/codec of the audio bytes.
    pub format: SttAudioFormat,
    /// BCP-47 language hint. `None` requests auto-detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Whether to attribute utterances to distinct speakers.
    #[serde(default = "default_diarize")]
    pub diarize: bool,
}

fn default_diarize() -> bool {
    true
}

/// Initial configuration a streaming STT session receives before audio frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SttStreamConfig {
    /// Audio format the client will push.
    pub format: SttAudioFormat,
    /// BCP-47 language hint. `None` requests auto-detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Whether to attribute utterances to distinct speakers.
    #[serde(default = "default_diarize")]
    pub diarize: bool,
}

/// Final transcript from a batch call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    /// Whole-transcript concatenated text.
    pub text: String,
    /// Per-utterance segments when diarization or timing is present.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<TranscriptSegment>,
    /// BCP-47 language code the provider detected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Total audio duration in seconds.
    pub duration_seconds: f64,
}

/// One utterance-shaped segment within a [`Transcript`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptSegment {
    /// Text of this segment.
    pub text: String,
    /// Start offset within the audio, in seconds.
    pub start_seconds: f64,
    /// End offset within the audio, in seconds.
    pub end_seconds: f64,
    /// Speaker identifier when diarized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
}

/// One incremental event from a streaming STT session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptEvent {
    /// Interim transcription hypothesis. May be replaced by a later event.
    Partial {
        /// Running text hypothesis.
        text: String,
    },
    /// Finalized utterance.
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
    /// End-of-stream marker with billable audio duration.
    Done {
        /// Billable session duration in seconds.
        duration_seconds: f64,
    },
}

/// Maximum number of speaker lanes Soniox documents for one session.
pub const MAX_SONIOX_SPEAKERS: u8 = 15;

/// One token emitted by a streaming STT provider.
#[derive(Clone, Debug, PartialEq)]
pub struct SttToken {
    /// Token text. May be a word, subword, punctuation, or provider marker.
    pub text: String,
    /// Token start time in milliseconds relative to session start.
    pub start_ms: Option<f64>,
    /// Token end time in milliseconds relative to session start.
    pub end_ms: Option<f64>,
    /// Recognition confidence from `0.0` to `1.0`.
    pub confidence: f32,
    /// `true` when this token is finalized and will not be revised.
    pub is_final: bool,
    /// Provider-specific diarization label.
    pub speaker_label: Option<String>,
}

/// Provider control marker derived from the token stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SttMarker {
    /// Semantic endpoint detected.
    Endpoint,
    /// Manual finalization completed.
    FinalizeComplete,
    /// Provider finished the stream.
    Finished,
}

/// Provider-neutral streaming event for token-native providers.
#[derive(Clone, Debug, PartialEq)]
pub enum SttStreamEvent {
    /// Token batch plus audio progress counters.
    Tokens {
        /// Tokens carried by this response.
        tokens: Vec<SttToken>,
        /// Audio processed into final tokens, in milliseconds.
        final_audio_proc_ms: Option<f64>,
        /// Audio processed into final plus non-final tokens, in milliseconds.
        total_audio_proc_ms: Option<f64>,
    },
    /// Control marker.
    Marker(SttMarker),
    /// Provider error response.
    Error {
        /// Provider status code when available.
        code: Option<u16>,
        /// Human-readable provider message. Must not contain secrets.
        message: String,
    },
    /// WebSocket close event.
    Closed {
        /// WebSocket close code.
        code: u16,
        /// WebSocket close reason.
        reason: String,
    },
}

/// Graph-ready update for one speaker lane from a token batch.
#[derive(Clone, Debug, PartialEq)]
pub struct SttGraphUpdate {
    /// Parley speaker lane index.
    pub lane: u8,
    /// Finalized words that should be appended exactly once.
    pub finalized: Vec<SttWord>,
    /// Current provisional words for this lane.
    pub provisional: Vec<SttWord>,
}

impl SttGraphUpdate {
    /// Apply this update to a [`WordGraph`].
    pub fn apply_to_graph(&self, graph: &mut WordGraph) {
        if !self.finalized.is_empty() {
            graph.ingest_turn(self.lane, &self.finalized, true);
        }
        if !self.provisional.is_empty() {
            graph.ingest_turn(self.lane, &self.provisional, false);
        }
    }
}

/// Normalized result from one provider event.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NormalizedSttBatch {
    /// Graph updates grouped by lane.
    pub updates: Vec<SttGraphUpdate>,
    /// Markers derived from provider control tokens or event metadata.
    pub markers: Vec<SttMarker>,
}

impl NormalizedSttBatch {
    /// True when this batch carries a provider boundary marker that should be
    /// treated as a committed turn boundary by UI consumers.
    pub fn has_turn_boundary(&self) -> bool {
        self.markers.iter().any(|marker| {
            matches!(
                marker,
                SttMarker::Endpoint | SttMarker::FinalizeComplete | SttMarker::Finished
            )
        })
    }

    /// Apply all graph updates in deterministic lane order.
    pub fn apply_to_graph(&self, graph: &mut WordGraph) {
        for update in &self.updates {
            update.apply_to_graph(graph);
        }
    }
}

/// Errors produced while normalizing provider token streams.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SttNormalizeError {
    /// The provider reported more diarized speakers than Parley can map.
    #[error("too many speaker labels for one STT session: {label}")]
    TooManySpeakers {
        /// Label that could not be assigned a lane.
        label: String,
    },
}

/// Stable per-session mapping from provider speaker labels to Parley lanes.
#[derive(Clone, Debug, Default)]
pub struct SpeakerLaneMap {
    labels: HashMap<String, u8>,
    next_lane: u8,
}

impl SpeakerLaneMap {
    /// Create an empty mapping.
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve a provider speaker label to a stable lane.
    pub fn lane_for(&mut self, label: Option<&str>) -> Result<u8, SttNormalizeError> {
        let Some(label) = label.filter(|value| !value.is_empty()) else {
            if self.next_lane == 0 {
                self.next_lane = 1;
            }
            return Ok(0);
        };

        if let Some(lane) = self.labels.get(label) {
            return Ok(*lane);
        }

        if self.next_lane >= MAX_SONIOX_SPEAKERS {
            return Err(SttNormalizeError::TooManySpeakers {
                label: label.to_string(),
            });
        }

        let lane = self.next_lane;
        self.labels.insert(label.to_string(), lane);
        self.next_lane += 1;
        Ok(lane)
    }
}

/// Stateful token normalizer for Soniox-compatible token streams.
///
/// Holds a per-lane "open partial word" for the *finalized* token stream so
/// that subword fragments split across consecutive WebSocket messages
/// (Soniox occasionally emits one word as `["speci", "es"]` across two
/// batches with no leading-space boundary on the continuation) get glued
/// back into a single word. The pending partial flushes when:
///
/// - The next batch's leading character for the same lane is whitespace
///   (Soniox signaled a real word boundary), **or**
/// - A control marker arrives ([`SttMarker::Endpoint`],
///   [`SttMarker::FinalizeComplete`], or [`SttMarker::Finished`]) — these
///   are turn / utterance boundaries and any held fragment is forced out.
///
/// Provisional tokens are re-rendered fresh each batch (Soniox re-sends the
/// full provisional state on every update), so they're not held across
/// batches.
#[derive(Clone, Debug, Default)]
pub struct TokenStreamNormalizer {
    lane_map: SpeakerLaneMap,
    /// Per-lane trailing partial finalized word. Carries forward across
    /// `accept_event` calls. See type-level docs for the flush rules.
    finalized_pending: BTreeMap<u8, PartialWord>,
}

impl TokenStreamNormalizer {
    /// Create an empty normalizer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Normalize one provider-neutral stream event into graph updates.
    pub fn accept_event(
        &mut self,
        event: SttStreamEvent,
    ) -> Result<NormalizedSttBatch, SttNormalizeError> {
        match event {
            SttStreamEvent::Tokens {
                tokens,
                final_audio_proc_ms,
                total_audio_proc_ms,
            } => self.accept_tokens(tokens, final_audio_proc_ms, total_audio_proc_ms),
            SttStreamEvent::Marker(marker) => {
                // A marker is a turn boundary: flush every lane's held
                // finalized partial as a complete word. This is what
                // prevents the last word of a turn from being silently
                // dropped if the user stops talking before Soniox sends a
                // trailing-space token.
                let mut batch = self.flush_pending_finalized();
                batch.markers.push(marker);
                Ok(batch)
            }
            SttStreamEvent::Error { .. } | SttStreamEvent::Closed { .. } => {
                Ok(NormalizedSttBatch::default())
            }
        }
    }

    fn accept_tokens(
        &mut self,
        tokens: Vec<SttToken>,
        final_audio_proc_ms: Option<f64>,
        total_audio_proc_ms: Option<f64>,
    ) -> Result<NormalizedSttBatch, SttNormalizeError> {
        let mut markers = Vec::new();
        let mut finalized_by_lane: BTreeMap<u8, Vec<SttToken>> = BTreeMap::new();
        let mut provisional_by_lane: BTreeMap<u8, Vec<SttToken>> = BTreeMap::new();

        for mut token in tokens {
            if token.text.is_empty() {
                continue;
            }
            match token.text.as_str() {
                "<end>" => {
                    markers.push(SttMarker::Endpoint);
                    continue;
                }
                "<fin>" => {
                    markers.push(SttMarker::FinalizeComplete);
                    continue;
                }
                _ => {}
            }

            let fallback_ms = if token.is_final {
                final_audio_proc_ms.or(total_audio_proc_ms)
            } else {
                total_audio_proc_ms.or(final_audio_proc_ms)
            };
            if token.start_ms.is_none() {
                token.start_ms = fallback_ms;
            }
            if token.end_ms.is_none() {
                token.end_ms = fallback_ms;
            }

            let lane = self.lane_map.lane_for(token.speaker_label.as_deref())?;
            if token.is_final {
                finalized_by_lane.entry(lane).or_default().push(token);
            } else {
                provisional_by_lane.entry(lane).or_default().push(token);
            }
        }

        // Lanes that need an emit decision: any with new tokens this
        // batch, OR any that are still carrying a pending partial from a
        // previous batch (so the marker-flush path covers them too — but
        // here we only emit on actual token arrival; markers are handled
        // separately above).
        let mut lanes: Vec<u8> = finalized_by_lane
            .keys()
            .chain(provisional_by_lane.keys())
            .copied()
            .collect();
        lanes.sort_unstable();
        lanes.dedup();

        let updates = lanes
            .into_iter()
            .map(|lane| {
                let finalized_tokens = finalized_by_lane
                    .get(&lane)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let provisional_tokens = provisional_by_lane
                    .get(&lane)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);

                // Continue the previous batch's pending partial word for
                // this lane (if any). After processing, store the new
                // trailing partial — it may continue into the *next*
                // batch.
                let prev_pending = self.finalized_pending.remove(&lane).unwrap_or_default();
                let (finalized, trailing) =
                    assemble_tokens_with_state(prev_pending, finalized_tokens, false);
                if !trailing.text.is_empty() {
                    self.finalized_pending.insert(lane, trailing);
                }

                // Provisional is always re-rendered fully each batch —
                // Soniox re-sends the entire provisional buffer with
                // every update.
                let (provisional, _) =
                    assemble_tokens_with_state(PartialWord::default(), provisional_tokens, true);

                SttGraphUpdate {
                    lane,
                    finalized,
                    provisional,
                }
            })
            .filter(|update| !update.finalized.is_empty() || !update.provisional.is_empty())
            .collect();

        Ok(NormalizedSttBatch { updates, markers })
    }

    /// Drain every lane's held finalized partial as a complete word,
    /// returning a batch with one [`SttGraphUpdate`] per non-empty lane.
    fn flush_pending_finalized(&mut self) -> NormalizedSttBatch {
        let mut updates = Vec::new();
        let pending = std::mem::take(&mut self.finalized_pending);
        let mut entries: Vec<_> = pending.into_iter().collect();
        // BTreeMap already iterates in lane order, but `into_iter` on the
        // moved map gives an arbitrary order in some std versions — sort
        // explicitly so the output is deterministic.
        entries.sort_by_key(|(lane, _)| *lane);
        for (lane, mut partial) in entries {
            if partial.text.is_empty() {
                continue;
            }
            let mut words = Vec::new();
            partial.flush(&mut words);
            updates.push(SttGraphUpdate {
                lane,
                finalized: words,
                provisional: Vec::new(),
            });
        }
        NormalizedSttBatch {
            updates,
            markers: Vec::new(),
        }
    }
}

/// Assemble a sequence of [`SttToken`]s into [`SttWord`]s, optionally
/// continuing from a previous batch's trailing partial word.
///
/// `flush_trailing = true` always emits the final partial as a complete
/// word (used for provisional rendering, which re-builds from scratch each
/// batch). `flush_trailing = false` returns the trailing partial so the
/// caller can carry it into the next batch (used for finalized tokens, see
/// [`TokenStreamNormalizer`]).
fn assemble_tokens_with_state(
    initial: PartialWord,
    tokens: &[SttToken],
    flush_trailing: bool,
) -> (Vec<SttWord>, PartialWord) {
    let mut words = Vec::new();
    let mut current = initial;

    for token in tokens {
        let start_ms = token.start_ms.unwrap_or(0.0);
        let end_ms = token.end_ms.unwrap_or(start_ms);
        for c in token.text.chars() {
            if c.is_whitespace() {
                current.flush(&mut words);
            } else if is_punctuation_token(c) {
                current.flush(&mut words);
                words.push(SttWord {
                    text: c.to_string(),
                    start_ms: end_ms,
                    end_ms,
                    confidence: token.confidence,
                    word_is_final: token.is_final,
                });
            } else {
                current.push(c, start_ms, end_ms, token.confidence, token.is_final);
            }
        }
    }

    if flush_trailing {
        current.flush(&mut words);
    }

    (words, current)
}

fn is_punctuation_token(c: char) -> bool {
    matches!(c, '.' | ',' | '?' | '!' | ';' | ':' | '"')
}

#[derive(Clone, Debug, Default)]
struct PartialWord {
    text: String,
    start_ms: Option<f64>,
    end_ms: Option<f64>,
    confidence: Option<f32>,
    word_is_final: bool,
}

impl PartialWord {
    fn push(&mut self, c: char, start_ms: f64, end_ms: f64, confidence: f32, is_final: bool) {
        if self.text.is_empty() {
            self.start_ms = Some(start_ms);
            self.confidence = Some(confidence);
            self.word_is_final = is_final;
        } else {
            self.confidence = Some(self.confidence.unwrap_or(confidence).min(confidence));
            self.word_is_final = self.word_is_final && is_final;
        }
        self.end_ms = Some(end_ms);
        self.text.push(c);
    }

    fn flush(&mut self, words: &mut Vec<SttWord>) {
        if self.text.is_empty() {
            return;
        }
        let start_ms = self.start_ms.unwrap_or(0.0);
        words.push(SttWord {
            text: std::mem::take(&mut self.text),
            start_ms,
            end_ms: self.end_ms.unwrap_or(start_ms),
            confidence: self.confidence.unwrap_or(1.0),
            word_is_final: self.word_is_final,
        });
        self.start_ms = None;
        self.end_ms = None;
        self.confidence = None;
        self.word_is_final = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::word_graph::NodeKind;

    fn token(text: &str, is_final: bool, speaker: Option<&str>, start: f64, end: f64) -> SttToken {
        SttToken {
            text: text.to_string(),
            start_ms: Some(start),
            end_ms: Some(end),
            confidence: 0.9,
            is_final,
            speaker_label: speaker.map(str::to_string),
        }
    }

    fn graph_texts(graph: &WordGraph, lane: u8) -> Vec<String> {
        graph
            .walk_spine(lane)
            .into_iter()
            .map(|id| graph.node(id).expect("node exists").text.clone())
            .collect()
    }

    #[test]
    fn audio_format_pcm_round_trip() {
        let f = SttAudioFormat::Pcm16Le {
            sample_rate_hz: 16000,
        };
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains("\"kind\":\"pcm16_le\""));
        let parsed: SttAudioFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, f);
    }

    #[test]
    fn diarize_defaults_to_true_on_deserialize() {
        let json = r#"{"audio":[1,2,3],"format":{"kind":"wav"}}"#;
        let req: SttRequest = serde_json::from_str(json).unwrap();
        assert!(req.diarize);
        assert!(req.language.is_none());
    }

    #[test]
    fn transcript_event_final_with_speaker_round_trip() {
        let event = TranscriptEvent::Final {
            text: "hello world".into(),
            speaker: Some("A".into()),
            start_seconds: Some(0.4),
            end_seconds: Some(1.8),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"kind\":\"final\""));
        assert_eq!(
            serde_json::from_str::<TranscriptEvent>(&json).unwrap(),
            event
        );
    }

    #[test]
    fn stream_config_round_trips_with_pcm() {
        let config = SttStreamConfig {
            format: SttAudioFormat::Pcm16Le {
                sample_rate_hz: 16000,
            },
            language: Some("en".into()),
            diarize: true,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: SttStreamConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn missing_speaker_label_maps_to_lane_zero() {
        let mut map = SpeakerLaneMap::new();
        assert_eq!(map.lane_for(None).unwrap(), 0);
        assert_eq!(map.lane_for(Some("")).unwrap(), 0);
    }

    #[test]
    fn speaker_labels_map_to_stable_lanes() {
        let mut map = SpeakerLaneMap::new();
        assert_eq!(map.lane_for(Some("1")).unwrap(), 0);
        assert_eq!(map.lane_for(Some("2")).unwrap(), 1);
        assert_eq!(map.lane_for(Some("1")).unwrap(), 0);
    }

    #[test]
    fn missing_label_reserves_lane_zero_before_labeled_speakers() {
        let mut map = SpeakerLaneMap::new();
        assert_eq!(map.lane_for(None).unwrap(), 0);
        assert_eq!(map.lane_for(Some("1")).unwrap(), 1);
    }

    #[test]
    fn too_many_speaker_labels_returns_error() {
        let mut map = SpeakerLaneMap::new();
        for i in 0..MAX_SONIOX_SPEAKERS {
            assert_eq!(map.lane_for(Some(&i.to_string())).unwrap(), i);
        }
        assert!(matches!(
            map.lane_for(Some("overflow")),
            Err(SttNormalizeError::TooManySpeakers { .. })
        ));
    }

    #[test]
    fn final_tokens_assemble_subwords_and_punctuation() {
        let mut normalizer = TokenStreamNormalizer::new();
        let batch = normalizer
            .accept_event(SttStreamEvent::Tokens {
                tokens: vec![
                    token("Beau", true, Some("1"), 0.0, 100.0),
                    token("ti", true, Some("1"), 100.0, 150.0),
                    token("ful", true, Some("1"), 150.0, 240.0),
                    token("!", true, Some("1"), 240.0, 250.0),
                ],
                final_audio_proc_ms: Some(250.0),
                total_audio_proc_ms: Some(250.0),
            })
            .unwrap();

        assert_eq!(batch.updates.len(), 1);
        assert_eq!(batch.updates[0].lane, 0);
        assert_eq!(
            batch.updates[0]
                .finalized
                .iter()
                .map(|word| word.text.as_str())
                .collect::<Vec<_>>(),
            vec!["Beautiful", "!"]
        );
    }

    #[test]
    fn marker_tokens_do_not_create_graph_updates() {
        let mut normalizer = TokenStreamNormalizer::new();
        let batch = normalizer
            .accept_event(SttStreamEvent::Tokens {
                tokens: vec![
                    token("<end>", true, Some("1"), 0.0, 0.0),
                    token("<fin>", true, Some("1"), 0.0, 0.0),
                ],
                final_audio_proc_ms: Some(0.0),
                total_audio_proc_ms: Some(0.0),
            })
            .unwrap();

        assert!(batch.updates.is_empty());
        assert_eq!(
            batch.markers,
            vec![SttMarker::Endpoint, SttMarker::FinalizeComplete]
        );
        assert!(batch.has_turn_boundary());
    }

    #[test]
    fn turn_boundary_detection_covers_provider_markers() {
        for marker in [
            SttMarker::Endpoint,
            SttMarker::FinalizeComplete,
            SttMarker::Finished,
        ] {
            let batch = NormalizedSttBatch {
                updates: Vec::new(),
                markers: vec![marker],
            };
            assert!(batch.has_turn_boundary(), "marker {marker:?} is a boundary");
        }

        assert!(!NormalizedSttBatch::default().has_turn_boundary());
    }

    #[test]
    fn final_and_provisional_tokens_apply_to_graph_without_duplication() {
        let mut normalizer = TokenStreamNormalizer::new();
        let mut graph = WordGraph::new();

        let first = normalizer
            .accept_event(SttStreamEvent::Tokens {
                tokens: vec![token("hel", false, Some("1"), 0.0, 100.0)],
                final_audio_proc_ms: Some(0.0),
                total_audio_proc_ms: Some(100.0),
            })
            .unwrap();
        first.apply_to_graph(&mut graph);
        assert_eq!(graph_texts(&graph, 0), vec!["hel"]);

        let second = normalizer
            .accept_event(SttStreamEvent::Tokens {
                tokens: vec![
                    token("hello", true, Some("1"), 0.0, 180.0),
                    token(" ", true, Some("1"), 180.0, 180.0),
                    token("wor", false, Some("1"), 180.0, 250.0),
                ],
                final_audio_proc_ms: Some(180.0),
                total_audio_proc_ms: Some(250.0),
            })
            .unwrap();
        second.apply_to_graph(&mut graph);
        assert_eq!(graph_texts(&graph, 0), vec!["hello", "wor"]);

        // The finalized "world" token has no leading whitespace, so the
        // normalizer holds it as a partial pending the next word boundary
        // or marker. This is the cross-batch fix: without an explicit
        // boundary, we must not emit it yet (it could continue).
        let third = normalizer
            .accept_event(SttStreamEvent::Tokens {
                tokens: vec![token("world", true, Some("1"), 180.0, 320.0)],
                final_audio_proc_ms: Some(320.0),
                total_audio_proc_ms: Some(320.0),
            })
            .unwrap();
        third.apply_to_graph(&mut graph);
        // "wor" provisional remains; "world" is still pending.
        assert_eq!(graph_texts(&graph, 0), vec!["hello", "wor"]);

        // A marker (turn endpoint) flushes the pending "world" as a
        // complete finalized word.
        let fourth = normalizer
            .accept_event(SttStreamEvent::Marker(SttMarker::Endpoint))
            .unwrap();
        fourth.apply_to_graph(&mut graph);
        assert_eq!(graph_texts(&graph, 0), vec!["hello", "world"]);
    }

    // ── Cross-batch subword continuation (Soniox space-insertion fix) ──

    #[test]
    fn cross_batch_finalized_subwords_merge_into_one_word() {
        // Soniox sometimes finalizes a single word as two subword tokens
        // delivered in separate WS messages, with no leading-space
        // boundary on the continuation. The normalizer must hold the
        // first fragment until the next batch's first character signals
        // a real word boundary (or a marker arrives).
        let mut normalizer = TokenStreamNormalizer::new();

        let b1 = normalizer
            .accept_event(SttStreamEvent::Tokens {
                tokens: vec![token("speci", true, Some("1"), 0.0, 200.0)],
                final_audio_proc_ms: Some(200.0),
                total_audio_proc_ms: Some(200.0),
            })
            .unwrap();
        // Pending — no completed word yet.
        assert!(b1.updates.is_empty());

        let b2 = normalizer
            .accept_event(SttStreamEvent::Tokens {
                tokens: vec![token("es", true, Some("1"), 200.0, 260.0)],
                final_audio_proc_ms: Some(260.0),
                total_audio_proc_ms: Some(260.0),
            })
            .unwrap();
        // Still pending — "speci" + "es" continue accumulating.
        assert!(b2.updates.is_empty());

        let b3 = normalizer
            .accept_event(SttStreamEvent::Marker(SttMarker::Endpoint))
            .unwrap();
        assert_eq!(b3.updates.len(), 1);
        assert_eq!(
            b3.updates[0]
                .finalized
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>(),
            vec!["species"]
        );
        assert_eq!(b3.markers, vec![SttMarker::Endpoint]);
    }

    #[test]
    fn cross_batch_leading_space_flushes_previous_pending() {
        // First batch finalizes "speci" with no trailing boundary.
        // Second batch's first token has a leading space, signalling a
        // real word boundary — the pending "speci" must flush, and the
        // new word "es" starts.
        let mut normalizer = TokenStreamNormalizer::new();

        let b1 = normalizer
            .accept_event(SttStreamEvent::Tokens {
                tokens: vec![token("speci", true, Some("1"), 0.0, 200.0)],
                final_audio_proc_ms: Some(200.0),
                total_audio_proc_ms: Some(200.0),
            })
            .unwrap();
        assert!(b1.updates.is_empty());

        let b2 = normalizer
            .accept_event(SttStreamEvent::Tokens {
                tokens: vec![token(" es", true, Some("1"), 200.0, 260.0)],
                final_audio_proc_ms: Some(260.0),
                total_audio_proc_ms: Some(260.0),
            })
            .unwrap();
        // The leading space flushes the previous pending "speci"; "es"
        // is the new pending word.
        assert_eq!(b2.updates.len(), 1);
        assert_eq!(
            b2.updates[0]
                .finalized
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>(),
            vec!["speci"]
        );

        let b3 = normalizer
            .accept_event(SttStreamEvent::Marker(SttMarker::FinalizeComplete))
            .unwrap();
        assert_eq!(
            b3.updates[0]
                .finalized
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>(),
            vec!["es"]
        );
    }

    #[test]
    fn cross_batch_pending_is_per_lane() {
        // Two speakers each have their own pending partial; they must
        // not bleed into one another.
        let mut normalizer = TokenStreamNormalizer::new();

        let _b1 = normalizer
            .accept_event(SttStreamEvent::Tokens {
                tokens: vec![
                    token("hel", true, Some("1"), 0.0, 100.0),
                    token("wo", true, Some("2"), 50.0, 120.0),
                ],
                final_audio_proc_ms: Some(120.0),
                total_audio_proc_ms: Some(120.0),
            })
            .unwrap();

        let _b2 = normalizer
            .accept_event(SttStreamEvent::Tokens {
                tokens: vec![
                    token("lo", true, Some("1"), 100.0, 180.0),
                    token("rld", true, Some("2"), 120.0, 220.0),
                ],
                final_audio_proc_ms: Some(220.0),
                total_audio_proc_ms: Some(220.0),
            })
            .unwrap();

        let b3 = normalizer
            .accept_event(SttStreamEvent::Marker(SttMarker::Endpoint))
            .unwrap();
        assert_eq!(b3.updates.len(), 2);
        let texts: Vec<(u8, Vec<&str>)> = b3
            .updates
            .iter()
            .map(|u| {
                (
                    u.lane,
                    u.finalized.iter().map(|w| w.text.as_str()).collect(),
                )
            })
            .collect();
        assert!(texts.contains(&(0, vec!["hello"])));
        assert!(texts.contains(&(1, vec!["world"])));
    }

    #[test]
    fn cross_batch_punctuation_in_continuation_flushes_pending() {
        // "speci" pending → "es," in next batch: the comma is
        // punctuation, which (per the assembler) flushes the current
        // word before being emitted as a stand-alone punctuation token.
        let mut normalizer = TokenStreamNormalizer::new();
        let _ = normalizer
            .accept_event(SttStreamEvent::Tokens {
                tokens: vec![token("speci", true, Some("1"), 0.0, 200.0)],
                final_audio_proc_ms: Some(200.0),
                total_audio_proc_ms: Some(200.0),
            })
            .unwrap();
        let b = normalizer
            .accept_event(SttStreamEvent::Tokens {
                tokens: vec![token("es,", true, Some("1"), 200.0, 260.0)],
                final_audio_proc_ms: Some(260.0),
                total_audio_proc_ms: Some(260.0),
            })
            .unwrap();
        assert_eq!(
            b.updates[0]
                .finalized
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>(),
            vec!["species", ","]
        );
    }

    #[test]
    fn cross_batch_finalize_complete_flushes_all_pending_lanes() {
        let mut normalizer = TokenStreamNormalizer::new();
        let _ = normalizer
            .accept_event(SttStreamEvent::Tokens {
                tokens: vec![
                    token("Encyclop", true, Some("1"), 0.0, 200.0),
                    token("Sub", true, Some("2"), 0.0, 200.0),
                ],
                final_audio_proc_ms: Some(200.0),
                total_audio_proc_ms: Some(200.0),
            })
            .unwrap();
        let _ = normalizer
            .accept_event(SttStreamEvent::Tokens {
                tokens: vec![
                    token("edia", true, Some("1"), 200.0, 260.0),
                    token("marines", true, Some("2"), 200.0, 280.0),
                ],
                final_audio_proc_ms: Some(280.0),
                total_audio_proc_ms: Some(280.0),
            })
            .unwrap();
        let flush = normalizer
            .accept_event(SttStreamEvent::Marker(SttMarker::FinalizeComplete))
            .unwrap();
        let texts: Vec<(u8, Vec<&str>)> = flush
            .updates
            .iter()
            .map(|u| {
                (
                    u.lane,
                    u.finalized.iter().map(|w| w.text.as_str()).collect(),
                )
            })
            .collect();
        assert!(texts.contains(&(0, vec!["Encyclopedia"])));
        assert!(texts.contains(&(1, vec!["Submarines"])));
        assert_eq!(flush.markers, vec![SttMarker::FinalizeComplete]);
    }

    #[test]
    fn one_response_updates_multiple_lanes() {
        let mut normalizer = TokenStreamNormalizer::new();
        // Tokens with no trailing whitespace are now held pending across
        // batches (cross-batch subword fix). Use a marker to force the
        // flush so the test still exercises the multi-lane path.
        let _ = normalizer
            .accept_event(SttStreamEvent::Tokens {
                tokens: vec![
                    token("Hi", true, Some("1"), 0.0, 100.0),
                    token("Yep", true, Some("2"), 50.0, 130.0),
                ],
                final_audio_proc_ms: Some(130.0),
                total_audio_proc_ms: Some(130.0),
            })
            .unwrap();
        let batch = normalizer
            .accept_event(SttStreamEvent::Marker(SttMarker::Endpoint))
            .unwrap();

        assert_eq!(batch.updates.len(), 2);
        let mut graph = WordGraph::new();
        batch.apply_to_graph(&mut graph);
        assert_eq!(graph_texts(&graph, 0), vec!["Hi"]);
        assert_eq!(graph_texts(&graph, 1), vec!["Yep"]);
    }

    #[test]
    fn punctuation_tokens_create_punctuation_nodes_after_graph_apply() {
        let mut normalizer = TokenStreamNormalizer::new();
        let batch = normalizer
            .accept_event(SttStreamEvent::Tokens {
                tokens: vec![
                    token("Ok", true, Some("1"), 0.0, 100.0),
                    token(".", true, Some("1"), 100.0, 110.0),
                ],
                final_audio_proc_ms: Some(110.0),
                total_audio_proc_ms: Some(110.0),
            })
            .unwrap();
        let mut graph = WordGraph::new();
        batch.apply_to_graph(&mut graph);
        let spine = graph.walk_spine(0);

        assert_eq!(graph.node(spine[0]).unwrap().kind, NodeKind::Word);
        assert_eq!(graph.node(spine[1]).unwrap().kind, NodeKind::Punctuation);
    }
}
