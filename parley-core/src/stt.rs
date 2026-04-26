//! Provider-neutral speech-to-text stream events and token normalization.
//!
//! Soniox `stt-rt-v4` streams token batches rather than turn-shaped messages.
//! This module keeps that token-native shape in the WASM-safe core crate so
//! browser providers, word-graph ingest, and tests can share one contract.

use std::collections::{BTreeMap, HashMap};

use crate::word_graph::{SttWord, WordGraph};

/// Maximum number of speaker lanes Soniox documents for one transcription
/// session. Lane indexes are `0..MAX_SONIOX_SPEAKERS`.
pub const MAX_SONIOX_SPEAKERS: u8 = 15;

/// One token emitted by a streaming STT provider.
#[derive(Clone, Debug, PartialEq)]
pub struct SttToken {
    /// Token text. May be a word, subword, whitespace, punctuation, or a
    /// provider control marker such as `<end>` / `<fin>`.
    pub text: String,
    /// Token start time in milliseconds relative to session start, if the
    /// provider supplied it.
    pub start_ms: Option<f64>,
    /// Token end time in milliseconds relative to session start, if the
    /// provider supplied it.
    pub end_ms: Option<f64>,
    /// Recognition confidence from `0.0` to `1.0`.
    pub confidence: f32,
    /// `true` when this token is finalized and will not be repeated or
    /// revised by the provider.
    pub is_final: bool,
    /// Provider-specific diarization label. Soniox uses strings such as
    /// `"1"`, `"2"`; missing labels map to lane 0.
    pub speaker_label: Option<String>,
}

/// Provider control marker derived from the token stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SttMarker {
    /// Semantic endpoint detected (`<end>` in Soniox).
    Endpoint,
    /// Manual finalization completed (`<fin>` in Soniox).
    FinalizeComplete,
    /// Provider finished the stream.
    Finished,
}

/// Provider-neutral streaming event.
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
        /// Human-readable provider message. Must not contain secret material.
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

/// Graph-ready update for one lane from a token batch.
#[derive(Clone, Debug, PartialEq)]
pub struct SttGraphUpdate {
    /// Parley speaker lane index.
    pub lane: u8,
    /// Finalized words that should be appended exactly once.
    pub finalized: Vec<SttWord>,
    /// Current provisional words for this lane. Replaces the previous
    /// provisional tail for the lane.
    pub provisional: Vec<SttWord>,
}

impl SttGraphUpdate {
    /// Apply this update to a [`WordGraph`] using the existing turn ingest
    /// primitive: finalized deltas are committed first, then the current
    /// provisional tail is attached as turn-locked nodes.
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

    /// Resolve a provider speaker label to a stable lane. Missing labels map
    /// to lane 0 and reserve it for unlabeled speech.
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
#[derive(Clone, Debug, Default)]
pub struct TokenStreamNormalizer {
    lane_map: SpeakerLaneMap,
}

impl TokenStreamNormalizer {
    /// Create an empty normalizer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Normalize one provider-neutral stream event into graph updates and
    /// control markers. Provider errors and WebSocket closes intentionally do
    /// not mutate graph state.
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
            SttStreamEvent::Marker(marker) => Ok(NormalizedSttBatch {
                updates: Vec::new(),
                markers: vec![marker],
            }),
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

        let mut lanes: Vec<u8> = finalized_by_lane
            .keys()
            .chain(provisional_by_lane.keys())
            .copied()
            .collect();
        lanes.sort_unstable();
        lanes.dedup();

        let updates = lanes
            .into_iter()
            .map(|lane| SttGraphUpdate {
                lane,
                finalized: assemble_tokens(
                    finalized_by_lane
                        .get(&lane)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]),
                ),
                provisional: assemble_tokens(
                    provisional_by_lane
                        .get(&lane)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]),
                ),
            })
            .filter(|update| !update.finalized.is_empty() || !update.provisional.is_empty())
            .collect();

        Ok(NormalizedSttBatch { updates, markers })
    }
}

fn assemble_tokens(tokens: &[SttToken]) -> Vec<SttWord> {
    let mut words = Vec::new();
    let mut current = PartialWord::default();

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

    current.flush(&mut words);
    words
}

fn is_punctuation_token(c: char) -> bool {
    matches!(c, '.' | ',' | '?' | '!' | ';' | ':' | '"')
}

#[derive(Default)]
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
        let update = &batch.updates[0];
        assert_eq!(update.lane, 0);
        assert_eq!(
            update
                .finalized
                .iter()
                .map(|word| word.text.as_str())
                .collect::<Vec<_>>(),
            vec!["Beautiful", "!"]
        );
        assert_eq!(update.finalized[0].start_ms, 0.0);
        assert_eq!(update.finalized[0].end_ms, 240.0);
    }

    #[test]
    fn leading_space_token_delimits_previous_word() {
        let mut normalizer = TokenStreamNormalizer::new();
        let batch = normalizer
            .accept_event(SttStreamEvent::Tokens {
                tokens: vec![
                    token("How", true, Some("1"), 0.0, 100.0),
                    token(" are", true, Some("1"), 100.0, 180.0),
                    token(" you", true, Some("1"), 180.0, 240.0),
                    token("?", true, Some("1"), 240.0, 250.0),
                ],
                final_audio_proc_ms: Some(250.0),
                total_audio_proc_ms: Some(250.0),
            })
            .unwrap();

        assert_eq!(
            batch.updates[0]
                .finalized
                .iter()
                .map(|word| word.text.as_str())
                .collect::<Vec<_>>(),
            vec!["How", "are", "you", "?"]
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
    }

    #[test]
    fn language_field_is_absent_from_provider_neutral_token() {
        let token = SttToken {
            text: "hola".to_string(),
            start_ms: Some(0.0),
            end_ms: Some(100.0),
            confidence: 0.9,
            is_final: true,
            speaker_label: Some("1".to_string()),
        };
        assert_eq!(token.text, "hola");
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

        let third = normalizer
            .accept_event(SttStreamEvent::Tokens {
                tokens: vec![token("world", true, Some("1"), 180.0, 320.0)],
                final_audio_proc_ms: Some(320.0),
                total_audio_proc_ms: Some(320.0),
            })
            .unwrap();
        third.apply_to_graph(&mut graph);
        assert_eq!(graph_texts(&graph, 0), vec!["hello", "world"]);
    }

    #[test]
    fn one_response_updates_multiple_lanes() {
        let mut normalizer = TokenStreamNormalizer::new();
        let batch = normalizer
            .accept_event(SttStreamEvent::Tokens {
                tokens: vec![
                    token("Hi", true, Some("1"), 0.0, 100.0),
                    token("Yep", true, Some("2"), 50.0, 130.0),
                ],
                final_audio_proc_ms: Some(130.0),
                total_audio_proc_ms: Some(130.0),
            })
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
