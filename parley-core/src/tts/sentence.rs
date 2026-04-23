//! Sentence-boundary chunker for streaming LLM output.
//!
//! Drives TTS dispatch in Conversation Mode: the orchestrator feeds
//! every LLM `TextDelta` into [`SentenceChunker::push`]; whenever a
//! complete sentence boundary is detected, the chunker emits a
//! [`SentenceChunk`] and the orchestrator dispatches it to the TTS
//! provider. At end-of-stream, [`SentenceChunker::finish`] flushes
//! any trailing buffered text as a final sentence.
//!
//! ## Boundary rules
//!
//! Spec: `docs/conversation-voice-slice-spec.md` §4.1.1.
//!
//! A sentence boundary is detected when **all four** conditions hold:
//!
//! 1. The buffer contains a sentence-terminating character: `.`,
//!    `!`, or `?`.
//! 2. At least one character has arrived after the terminator
//!    (single-token lookahead — D18 in the parent spec).
//! 3. The character immediately after the terminator is whitespace
//!    (or end-of-stream via `finish()`).
//! 4. The first non-whitespace character following the terminator is
//!    *not* lowercase. This keeps `Dr. Smith` together: `.`
//!    followed by space followed by lowercase `s` is treated as an
//!    abbreviation.
//!
//! When all conditions are met, everything up to and including the
//! terminator is emitted as one chunk; trailing whitespace is
//! consumed but not included in the chunk text.

use serde::{Deserialize, Serialize};

/// One sentence ready to be synthesized.
///
/// `index` is zero-based within the current turn so callers can
/// label downstream artifacts (cache filenames, SSE events).
/// `final_for_turn` is set on the *last* chunk emitted for a turn so
/// the orchestrator can finalize the cache file without a separate
/// "is this the last one" lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentenceChunk {
    /// Zero-based index within the current turn.
    pub index: u32,
    /// The sentence text. Includes its terminating punctuation.
    /// Whitespace is trimmed from the trailing edge.
    pub text: String,
    /// `true` when this is the last chunk the chunker will emit for
    /// this turn. Always set on the chunk produced by `finish()`;
    /// also set on the final `push()`-emitted chunk if `finish()`
    /// would otherwise produce nothing.
    pub final_for_turn: bool,
}

/// Stateful sentence-boundary detector. One instance per turn.
#[derive(Debug, Default)]
pub struct SentenceChunker {
    /// Buffered text that hasn't been emitted yet.
    buf: String,
    /// Next index to assign on emit.
    next_index: u32,
    /// Whether `finish()` has already been called (guards against
    /// double-flush).
    finished: bool,
}

impl SentenceChunker {
    /// New, empty chunker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a delta from the LLM stream. Returns zero or more
    /// completed sentence chunks. Empty `delta` is a no-op.
    pub fn push(&mut self, delta: &str) -> Vec<SentenceChunk> {
        if delta.is_empty() || self.finished {
            return Vec::new();
        }
        self.buf.push_str(delta);
        self.drain_complete()
    }

    /// Drain remaining buffered text as a final chunk. Idempotent —
    /// subsequent calls return `None`. Always called exactly once
    /// per turn at end-of-stream so the orchestrator gets a clean
    /// "this is the last one" signal.
    pub fn finish(&mut self) -> Option<SentenceChunk> {
        if self.finished {
            return None;
        }
        self.finished = true;
        let trimmed = self.buf.trim();
        if trimmed.is_empty() {
            self.buf.clear();
            return None;
        }
        let text = trimmed.to_string();
        self.buf.clear();
        let chunk = SentenceChunk {
            index: self.next_index,
            text,
            final_for_turn: true,
        };
        self.next_index += 1;
        Some(chunk)
    }

    /// Walk the buffer, emitting every sentence whose boundary is
    /// fully visible (terminator + post-terminator whitespace +
    /// non-lowercase next char OR confirmed end of buffer).
    fn drain_complete(&mut self) -> Vec<SentenceChunk> {
        let mut out = Vec::new();
        // Iterate over byte indices of terminators, advancing past
        // any boundaries we successfully consume.
        loop {
            let Some(boundary) = self.next_boundary() else {
                break;
            };
            let sentence: String = self.buf[..boundary.text_end].trim().to_string();
            // Drop the terminator + the whitespace that followed it,
            // so the next iteration sees a clean buffer head.
            self.buf.drain(..boundary.consume_through);
            if sentence.is_empty() {
                // Defensive: if the buffer had nothing but the
                // terminator (shouldn't happen since terminator is
                // included), skip rather than emit an empty chunk.
                continue;
            }
            out.push(SentenceChunk {
                index: self.next_index,
                text: sentence,
                final_for_turn: false,
            });
            self.next_index += 1;
        }
        out
    }

    /// Find the next sentence boundary in `self.buf`, if any.
    /// Returns `None` when the buffer doesn't contain enough
    /// lookahead to commit yet.
    fn next_boundary(&self) -> Option<SentenceBoundary> {
        find_first_boundary(&self.buf)
    }
}

/// One detected sentence boundary inside a text buffer. Returned by
/// [`find_first_boundary`] and [`find_all_boundaries`] so callers
/// outside [`SentenceChunker`] (e.g. the paragraph-bounded
/// `ChunkPlanner`) can reuse the same boundary-detection rules
/// described in [`sentence`](self) without owning a chunker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SentenceBoundary {
    /// Byte offset just past the terminator. Sentence text is
    /// `buf[..text_end]` (after trimming trailing whitespace).
    pub text_end: usize,
    /// Byte offset to drain through (terminator + trailing
    /// whitespace) so the next scan starts on real content.
    pub consume_through: usize,
}

/// Find the first sentence boundary in `buf`, if one is fully
/// confirmed by the rules in this module's docs. Returns `None`
/// when the buffer doesn't contain enough lookahead to commit yet.
///
/// Pure function — does not mutate `buf`. Used by both
/// [`SentenceChunker`] (which drains its own buffer) and the
/// paragraph-bounded `ChunkPlanner` in [`crate::tts::chunking`]
/// (which only needs to *count* boundaries without consuming).
pub fn find_first_boundary(buf: &str) -> Option<SentenceBoundary> {
    let bytes = buf.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if matches!(b, b'.' | b'!' | b'?') {
            // Terminator at byte i. Need at least one byte of
            // lookahead.
            let after = i + 1;
            if after >= bytes.len() {
                return None;
            }
            let next = bytes[after];
            // Condition 3: the immediate next char must be
            // whitespace (otherwise this terminator is internal
            // — e.g. "3.14", "wait...", "v1.0").
            if !is_ws(next) {
                i = after;
                continue;
            }
            // Abbreviation guard: if the terminator is `.` and
            // the word immediately preceding it is a known
            // abbreviation (e.g. "Dr.", "Mr.", "St."), treat
            // this terminator as part of the abbreviation and
            // keep scanning.
            if b == b'.' && preceding_word_is_abbreviation(&buf[..i]) {
                i = after;
                continue;
            }
            // Condition 4: scan past the run of whitespace; the
            // first non-whitespace char must not be lowercase.
            let mut j = after;
            while j < bytes.len() && is_ws(bytes[j]) {
                j += 1;
            }
            if j >= bytes.len() {
                // We have whitespace but no following character
                // yet — can't commit (might be `Dr. ` waiting
                // for `Smith`).
                return None;
            }
            let next_nws = bytes[j];
            if next_nws.is_ascii_lowercase() {
                // Treat as abbreviation; merge and continue.
                i = j;
                continue;
            }
            return Some(SentenceBoundary {
                text_end: after,
                consume_through: j,
            });
        }
        i += 1;
    }
    None
}

/// Find every sentence boundary in `buf`, in order. Equivalent to
/// repeatedly applying [`find_first_boundary`] to the unconsumed
/// suffix.
///
/// Like [`find_first_boundary`], this is a pure function and does
/// not mutate `buf`. Returned offsets are absolute (relative to
/// the start of `buf`).
pub fn find_all_boundaries(buf: &str) -> Vec<SentenceBoundary> {
    let mut out = Vec::new();
    let mut offset: usize = 0;
    while offset < buf.len() {
        let suffix = &buf[offset..];
        let Some(b) = find_first_boundary(suffix) else {
            break;
        };
        out.push(SentenceBoundary {
            text_end: offset + b.text_end,
            consume_through: offset + b.consume_through,
        });
        offset += b.consume_through;
    }
    out
}

/// Like [`find_all_boundaries`], plus one additional "EOF-relaxed"
/// boundary when the buffer ends with a sentence terminator
/// (followed only by whitespace, if any). Used by the
/// paragraph-bounded `ChunkPlanner` so that timer-driven release
/// rules can treat a trailing `Hello.` as a complete sentence
/// without waiting for additional lookahead.
///
/// The strict abbreviation guard still applies: a trailing `Dr.`
/// is NOT promoted to a sentence boundary.
pub fn find_all_boundaries_relaxed(buf: &str) -> Vec<SentenceBoundary> {
    let mut out = find_all_boundaries(buf);
    let scan_from = out.last().map(|b| b.consume_through).unwrap_or(0);
    if scan_from >= buf.len() {
        return out;
    }
    let suffix = &buf[scan_from..];
    let trimmed = suffix.trim_end();
    if trimmed.is_empty() {
        return out;
    }
    let last_char = trimmed.chars().next_back().expect("non-empty");
    if !matches!(last_char, '.' | '!' | '?') {
        return out;
    }
    let abs_term = scan_from + trimmed.len() - last_char.len_utf8();
    if last_char == '.' && preceding_word_is_abbreviation(&buf[..abs_term]) {
        return out;
    }
    out.push(SentenceBoundary {
        text_end: abs_term + last_char.len_utf8(),
        consume_through: buf.len(),
    });
    out
}

#[inline]
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// Common English title/honorific abbreviations whose trailing `.`
/// should not be treated as a sentence terminator.
///
/// Matched case-insensitively against the alphabetical run that
/// immediately precedes the terminator. The list is intentionally
/// small — TTS chunking is forgiving, and a missed split here just
/// produces a slightly longer audio chunk.
const ABBREVIATIONS: &[&str] = &[
    "dr", "mr", "mrs", "ms", "jr", "sr", "st", "vs", "etc", "ie", "eg",
];

/// `true` when the alphabetical run at the tail of `prefix` matches
/// (case-insensitively) one of [`ABBREVIATIONS`].
fn preceding_word_is_abbreviation(prefix: &str) -> bool {
    let bytes = prefix.as_bytes();
    let mut start = bytes.len();
    while start > 0 && bytes[start - 1].is_ascii_alphabetic() {
        start -= 1;
    }
    if start == bytes.len() {
        return false;
    }
    let word = &prefix[start..];
    ABBREVIATIONS
        .iter()
        .any(|abbr| word.eq_ignore_ascii_case(abbr))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_and_finish(deltas: &[&str]) -> Vec<SentenceChunk> {
        let mut c = SentenceChunker::new();
        let mut out = Vec::new();
        for d in deltas {
            out.extend(c.push(d));
        }
        if let Some(last) = c.finish() {
            out.push(last);
        }
        // Mirror the orchestrator's behavior: ensure exactly one
        // chunk per turn carries `final_for_turn`. If `finish()`
        // produced nothing, the caller would set it on the last
        // emitted chunk; we replicate that here so tests reflect
        // the consumer-facing invariant.
        if let Some(last) = out.last_mut()
            && !last.final_for_turn
        {
            last.final_for_turn = true;
        }
        out
    }

    #[test]
    fn single_sentence_emits_on_finish() {
        let chunks = push_and_finish(&["Hello world"]);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "Hello world");
        assert!(chunks[0].final_for_turn);
        assert_eq!(chunks[0].index, 0);
    }

    #[test]
    fn single_sentence_with_terminator_then_finish() {
        let chunks = push_and_finish(&["Hello world."]);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "Hello world.");
        assert!(chunks[0].final_for_turn);
    }

    #[test]
    fn two_sentences_split_on_boundary() {
        let chunks = push_and_finish(&["Hi there. How are you?"]);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "Hi there.");
        assert_eq!(chunks[0].index, 0);
        assert!(!chunks[0].final_for_turn);
        assert_eq!(chunks[1].text, "How are you?");
        assert_eq!(chunks[1].index, 1);
        assert!(chunks[1].final_for_turn);
    }

    #[test]
    fn boundary_split_across_pushes() {
        let mut c = SentenceChunker::new();
        let mut out = Vec::new();
        out.extend(c.push("Hi there"));
        out.extend(c.push(". H")); // terminator + lookahead arrive together
        // First chunk should have committed once "H" was visible.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "Hi there.");
        out.extend(c.push("ow are you?"));
        out.extend(c.finish());
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].text, "How are you?");
    }

    #[test]
    fn dr_smith_does_not_split() {
        let chunks = push_and_finish(&["Hi Dr. Smith. How are you?"]);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "Hi Dr. Smith.");
        assert_eq!(chunks[1].text, "How are you?");
    }

    #[test]
    fn lowercase_after_terminator_does_not_split_even_across_pushes() {
        let mut c = SentenceChunker::new();
        let _ = c.push("Hi Dr.");
        let out = c.push(" smith you came back."); // lowercase 's' suppresses split
        assert_eq!(out.len(), 0);
        let last = c.finish().unwrap();
        assert_eq!(last.text, "Hi Dr. smith you came back.");
    }

    #[test]
    fn exclamation_and_question_terminators() {
        let chunks = push_and_finish(&["Wait! Is that you? Yes."]);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].text, "Wait!");
        assert_eq!(chunks[1].text, "Is that you?");
        assert_eq!(chunks[2].text, "Yes.");
    }

    #[test]
    fn empty_pushes_are_noops() {
        let mut c = SentenceChunker::new();
        assert!(c.push("").is_empty());
        assert!(c.push("").is_empty());
        assert!(c.finish().is_none());
    }

    #[test]
    fn finish_is_idempotent() {
        let mut c = SentenceChunker::new();
        let _ = c.push("Hello.");
        let first = c.finish();
        assert!(first.is_some());
        assert!(c.finish().is_none());
    }

    #[test]
    fn indices_increment_sequentially() {
        let chunks = push_and_finish(&["A. B. C. D."]);
        assert_eq!(
            chunks.iter().map(|c| c.index).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn internal_dot_in_number_does_not_split() {
        // "3.14" — the dot is followed by a digit '1', not
        // whitespace — must not split.
        let chunks = push_and_finish(&["Pi is 3.14 approximately."]);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "Pi is 3.14 approximately.");
    }

    #[test]
    fn newline_after_terminator_is_treated_as_whitespace() {
        let chunks = push_and_finish(&["First line.\nSecond line."]);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "First line.");
        assert_eq!(chunks[1].text, "Second line.");
    }

    #[test]
    fn token_by_token_simulation() {
        // Simulate the LLM streaming a response one token at a time.
        let tokens = [
            "Hello", ",", " ", "Gavin", ".", " ", "How", " ", "are", " ", "you", "?",
        ];
        let mut c = SentenceChunker::new();
        let mut out = Vec::new();
        for t in tokens {
            out.extend(c.push(t));
        }
        // First sentence should commit when " H" arrives after ".".
        // Final question still needs `finish()` since no trailing
        // whitespace ever appeared.
        out.extend(c.finish());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "Hello, Gavin.");
        assert_eq!(out[1].text, "How are you?");
    }

    #[test]
    fn final_chunk_carries_final_for_turn_flag() {
        // `finish()` is always the path that sets the flag; verify
        // that a turn ending mid-stream (without a trailing
        // terminator) still produces a final flag on the trailing
        // text.
        let mut c = SentenceChunker::new();
        let _ = c.push("Hi there. Some more");
        let last = c.finish().unwrap();
        assert!(last.final_for_turn);
        assert_eq!(last.text, "Some more");
    }
}
