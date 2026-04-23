//! Paragraph-bounded chunk planner for streaming TTS.
//!
//! [`ChunkPlanner`] is the state machine that turns LLM `TextDelta`
//! events into [`ReleasedChunk`]s sized for natural-sounding TTS
//! synthesis. Where [`crate::tts::SentenceChunker`] dispatches one
//! synthesis request per *sentence*, the planner targets one
//! request per *paragraph* (or near-paragraph), so the synthesizer
//! reads ahead far enough to modulate prosody across clauses.
//!
//! Spec: [`docs/paragraph-tts-chunking-spec.md`](../../../../docs/paragraph-tts-chunking-spec.md)
//! \u00a73 (architecture) and \u00a74 (data model).
//!
//! ## Release rules
//!
//! On every [`ChunkPlanner::push`] / [`ChunkPlanner::tick`] /
//! [`ChunkPlanner::synthesis_completed`] call the planner walks
//! these rules in order and releases at most one chunk per call
//! (single-flight \u2014 \u00a73.6). [`ChunkPlanner::finish`] bypasses
//! single-flight and drains everything left in the buffer.
//!
//! - **R4** Hard char cap (`pending.len() >= hard_cap_chars`): cut
//!   at the latest sentence boundary, else latest whitespace, else
//!   exactly at the cap.
//! - **R2** Paragraph break (`\n\n`): cut at the break, with
//!   `[paragraph + list]` grouping when a list marker follows the
//!   first break.
//! - **R1** First-chunk fast path: when no chunk has been released
//!   yet, release as soon as `first_chunk_max_sentences` are
//!   buffered, OR (fallback) when one sentence has been buffered
//!   for `first_chunk_max_wait_ms`.
//! - **R5** Idle timeout: when the LLM hasn't pushed for
//!   `idle_timeout_ms` and at least one sentence is buffered, cut
//!   at the latest sentence boundary.
//! - **R3** Paragraph-wait timer: when the wait window since
//!   buffering started exceeds `paragraph_wait_ms`, cut at the
//!   latest sentence boundary; if no sentence boundary exists yet,
//!   wait an additional `sentence_grace_ms` then cut at the latest
//!   whitespace.
//! - **R6** Stream end (via [`ChunkPlanner::finish`]): drain all
//!   remaining `\n\n`-delimited paragraphs as separate chunks; the
//!   last carries `final_for_turn = true`.
//!
//! ## Time
//!
//! Time is passed explicitly as a `now_ms: u64` argument (Unix
//! milliseconds). Keeps the planner pure and trivially testable
//! without a `Clock` trait.

use serde::{Deserialize, Serialize};

use crate::tts::sentence::{SentenceBoundary, find_all_boundaries_relaxed};

/// One released chunk ready for TTS synthesis dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleasedChunk {
    /// Zero-based index within the current turn.
    pub index: u32,
    /// The chunk's text. Whitespace is trimmed from both ends.
    pub text: String,
    /// `true` when this is the final chunk the planner will emit
    /// for the current turn (always set on the last chunk produced
    /// by [`ChunkPlanner::finish`]).
    pub final_for_turn: bool,
}

/// Tunable knobs for [`ChunkPlanner`]. Defaults are calibrated for
/// ElevenLabs v3 + 128 kbps MP3 playback; see the chunking spec
/// \u00a75 for rationale.
///
/// Each field carries a `#[serde(default = "...")]` so on-disk
/// configs can override individual knobs without restating every
/// value. A missing `[model.tts_chunking]` section in TOML still
/// yields `ChunkPolicy::default()` via the `#[serde(default)]` on
/// the parent field.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChunkPolicy {
    /// Sentence count that triggers the first-chunk release (R1).
    /// Default: `2`. The first chunk prefers two sentences for
    /// downstream buffer runway; see spec \u00a75.
    pub first_chunk_max_sentences: u32,
    /// Wait window in milliseconds after the *first* sentence
    /// completes before falling back to a one-sentence first chunk
    /// (R1 fallback). Default: `800`.
    pub first_chunk_max_wait_ms: u64,
    /// Wait window in milliseconds since pending text buffering
    /// began before the paragraph-wait timer (R3) fires.
    /// Default: `3000`.
    pub paragraph_wait_ms: u64,
    /// Additional milliseconds to wait after `paragraph_wait_ms`
    /// when no sentence boundary exists in the buffer (R3 grace).
    /// Default: `1000`.
    pub sentence_grace_ms: u64,
    /// Hard upper bound on pending buffer size in bytes before R4
    /// fires. Default: `1500`.
    pub hard_cap_chars: usize,
    /// Wait window in milliseconds since the last token push
    /// before R5 (idle timeout) fires. Default: `1500`.
    pub idle_timeout_ms: u64,
    /// Silence duration in milliseconds spliced before each
    /// non-first chunk's audio. Default: `500`. Consumed by the
    /// proxy-side `SilenceSplicer`; the planner only carries it.
    pub paragraph_silence_ms: u32,
    /// Silence duration in milliseconds spliced before the very
    /// first chunk of a turn. Default: `100`. Consumed by the
    /// proxy-side `SilenceSplicer`.
    pub first_chunk_silence_ms: u32,
}

impl Default for ChunkPolicy {
    fn default() -> Self {
        Self {
            first_chunk_max_sentences: 2,
            first_chunk_max_wait_ms: 800,
            paragraph_wait_ms: 3_000,
            sentence_grace_ms: 1_000,
            hard_cap_chars: 1_500,
            idle_timeout_ms: 1_500,
            paragraph_silence_ms: 500,
            first_chunk_silence_ms: 100,
        }
    }
}

/// State machine that turns LLM text deltas into released chunks.
///
/// One instance per turn. See module-level docs for the release
/// rules and single-flight backpressure model.
#[derive(Debug)]
pub struct ChunkPlanner {
    policy: ChunkPolicy,
    /// Accumulated text since the last released chunk.
    pending: String,
    /// Next chunk index to assign on release.
    next_index: u32,
    /// Total chunks already released (== `next_index`, kept for
    /// readability).
    chunks_released: u32,
    /// When the current wait period began. `None` between releases
    /// when `pending` is empty; set to the push time when text
    /// first arrives in an empty buffer.
    wait_started_at: Option<u64>,
    /// When the first complete sentence appeared in `pending`.
    /// Used only by the R1 one-sentence fallback. Reset on every
    /// release.
    first_boundary_seen_at: Option<u64>,
    /// Last `now_ms` at which a non-empty push arrived. Drives R5
    /// (idle timeout).
    last_token_at: u64,
    /// `true` while a previously released chunk is still being
    /// synthesized. Blocks all release rules except R6.
    synthesis_in_flight: bool,
    /// `true` once [`Self::finish`] has been called. Subsequent
    /// `push`/`tick`/`finish` calls become no-ops.
    finished: bool,
}

impl ChunkPlanner {
    /// Build a planner with the given policy.
    pub fn new(policy: ChunkPolicy) -> Self {
        Self {
            policy,
            pending: String::new(),
            next_index: 0,
            chunks_released: 0,
            wait_started_at: None,
            first_boundary_seen_at: None,
            last_token_at: 0,
            synthesis_in_flight: false,
            finished: false,
        }
    }

    /// Push token text. Returns the chunks released by this call.
    /// Empty text is a no-op. Single-flight (one in-flight
    /// synthesis at a time) means at most one chunk is released
    /// per call after the first.
    pub fn push(&mut self, text: &str, now_ms: u64) -> Vec<ReleasedChunk> {
        if text.is_empty() || self.finished {
            return Vec::new();
        }
        let was_idle = self.pending.is_empty();
        self.pending.push_str(text);
        self.last_token_at = now_ms;
        if was_idle && !self.pending.is_empty() {
            self.wait_started_at = Some(now_ms);
        }
        // R1 one-sentence fallback timer: marks when the first
        // confirmed sentence first appeared in pending. Idempotent
        // — set once, cleared on release. Uses relaxed boundary
        // detection so a trailing terminator counts even without
        // lookahead from the next token.
        if self.first_boundary_seen_at.is_none()
            && self.chunks_released == 0
            && !find_all_boundaries_relaxed(&self.pending).is_empty()
        {
            self.first_boundary_seen_at = Some(now_ms);
        }
        self.drain_releases(now_ms)
    }

    /// Advance the planner against the wall clock without pushing
    /// new text. Used by the orchestrator to fire the timer-based
    /// release rules (R1 fallback, R3, R5) when the LLM has gone
    /// quiet.
    pub fn tick(&mut self, now_ms: u64) -> Vec<ReleasedChunk> {
        if self.finished {
            return Vec::new();
        }
        self.drain_releases(now_ms)
    }

    /// Signal that a previously released chunk's synthesis has
    /// completed. Clears single-flight backpressure and may release
    /// queued content immediately.
    pub fn synthesis_completed(&mut self, _chunk_index: u32, now_ms: u64) -> Vec<ReleasedChunk> {
        self.synthesis_in_flight = false;
        if self.finished {
            return Vec::new();
        }
        self.drain_releases(now_ms)
    }

    /// Stream end. Drain `pending` as paragraph-bounded chunks
    /// regardless of single-flight, marking the last chunk
    /// `final_for_turn = true`. Idempotent \u2014 subsequent calls
    /// return an empty `Vec`.
    pub fn finish(&mut self, now_ms: u64) -> Vec<ReleasedChunk> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut out = Vec::new();
        // Emit one chunk per paragraph break still in pending.
        while let Some(cut) = paragraph_break_cut(&self.pending) {
            out.push(self.cut_at(now_ms, cut, false));
        }
        // Whatever remains is the final chunk.
        if !self.pending.trim().is_empty() {
            let cut = self.pending.len();
            out.push(self.cut_at(now_ms, cut, true));
        } else if let Some(last) = out.last_mut() {
            last.final_for_turn = true;
        }
        out
    }

    /// Whether the planner currently has a synthesis request
    /// awaiting completion. Exposed so the orchestrator can decide
    /// whether to fire `tick` proactively. Mostly useful in tests.
    pub fn synthesis_in_flight(&self) -> bool {
        self.synthesis_in_flight
    }

    // ---- internals --------------------------------------------------

    /// Try to release one chunk. Returns `None` when no rule fires.
    /// Honors single-flight: returns `None` if a previous chunk's
    /// synthesis is still pending (R6 / `finish` is the only path
    /// that bypasses this; it doesn't go through `try_release`).
    fn try_release(&mut self, now_ms: u64) -> Option<ReleasedChunk> {
        if self.pending.trim().is_empty() {
            return None;
        }
        if self.synthesis_in_flight {
            return None;
        }
        let policy = self.policy;
        let pending = &self.pending;

        // R4: hard cap. Most urgent \u2014 evaluated first to bound
        // memory/synthesis latency.
        if pending.len() >= policy.hard_cap_chars {
            let cut = latest_sentence_cut(pending)
                .or_else(|| latest_whitespace_cut(pending))
                .unwrap_or_else(|| char_floor(pending, policy.hard_cap_chars));
            return Some(self.cut_at(now_ms, cut, false));
        }

        // R2: paragraph break (with list grouping).
        // (Moved below R1 so R1's first-chunk fast path beats R2.)

        let boundaries = find_all_boundaries_relaxed(pending);
        let sentence_count = boundaries.len() as u32;

        // R1: first-chunk fast path. Higher priority than R2 so the
        // first chunk reliably reaches TTS as soon as TTFA budget
        // allows, even when a paragraph break is also visible in
        // pending (subsequent chunks happily release on R2).
        if self.chunks_released == 0 {
            if policy.first_chunk_max_sentences > 0
                && sentence_count >= policy.first_chunk_max_sentences
            {
                let idx = (policy.first_chunk_max_sentences as usize) - 1;
                let cut = boundaries[idx].consume_through;
                return Some(self.cut_at(now_ms, cut, false));
            }
            if sentence_count == 1
                && policy.first_chunk_max_sentences >= 2
                && let Some(t0) = self.first_boundary_seen_at
                && now_ms.saturating_sub(t0) >= policy.first_chunk_max_wait_ms
            {
                let cut = boundaries[0].consume_through;
                return Some(self.cut_at(now_ms, cut, false));
            }
        }

        // R2 (after R1): paragraph break (with list grouping).
        if let Some(cut) = paragraph_break_cut(pending) {
            return Some(self.cut_at(now_ms, cut, false));
        }

        // R5: idle timeout.
        if sentence_count >= 1
            && now_ms.saturating_sub(self.last_token_at) >= policy.idle_timeout_ms
        {
            let cut = boundaries.last().unwrap().consume_through;
            return Some(self.cut_at(now_ms, cut, false));
        }

        // R3: paragraph-wait timer (with grace fallback).
        if let Some(t0) = self.wait_started_at {
            let waited = now_ms.saturating_sub(t0);
            if waited >= policy.paragraph_wait_ms {
                if sentence_count >= 1 {
                    let cut = boundaries.last().unwrap().consume_through;
                    return Some(self.cut_at(now_ms, cut, false));
                } else if waited >= policy.paragraph_wait_ms + policy.sentence_grace_ms {
                    let cut = latest_whitespace_cut(pending).unwrap_or(pending.len());
                    return Some(self.cut_at(now_ms, cut, false));
                }
            }
        }

        None
    }

    /// Apply `try_release` until no more rules fire. Single-flight
    /// caps this at one release per call after the first \u2014 once a
    /// chunk releases, `synthesis_in_flight` flips and the next
    /// iteration short-circuits.
    fn drain_releases(&mut self, now_ms: u64) -> Vec<ReleasedChunk> {
        let mut out = Vec::new();
        while let Some(chunk) = self.try_release(now_ms) {
            out.push(chunk);
        }
        out
    }

    /// Emit the chunk consisting of `pending[..cut]`, drain
    /// `pending` past the cut and any leading whitespace that
    /// remains, and update bookkeeping (next index, single-flight
    /// flag, wait timers).
    fn cut_at(&mut self, now_ms: u64, cut: usize, final_for_turn: bool) -> ReleasedChunk {
        let mut cut = cut.min(self.pending.len());
        // Be defensive: never cut inside a UTF-8 codepoint.
        while cut > 0 && !self.pending.is_char_boundary(cut) {
            cut -= 1;
        }
        let chunk_text: String = self.pending[..cut].trim().to_string();
        self.pending.drain(..cut);
        // Strip any leading whitespace so the next chunk starts on
        // real content (the cut may have stopped before consuming a
        // trailing whitespace run).
        let leading = self.pending.len() - self.pending.trim_start().len();
        if leading > 0 {
            self.pending.drain(..leading);
        }

        let chunk = ReleasedChunk {
            index: self.next_index,
            text: chunk_text,
            final_for_turn,
        };
        self.next_index += 1;
        self.chunks_released += 1;
        self.synthesis_in_flight = true;
        self.first_boundary_seen_at = None;
        self.wait_started_at = if self.pending.is_empty() {
            None
        } else {
            // Buffered text remains; its wait window starts now.
            Some(now_ms)
        };
        chunk
    }
}

// ---- free helpers --------------------------------------------------

/// Latest sentence-cut position in `text`: the `consume_through`
/// of the last confirmed boundary (relaxed: a trailing terminator
/// counts), or `None` if there is none.
fn latest_sentence_cut(text: &str) -> Option<usize> {
    find_all_boundaries_relaxed(text)
        .last()
        .map(|b: &SentenceBoundary| b.consume_through)
}

/// Position just past the latest whitespace character in `text`,
/// or `None` if there is no whitespace. Cuts there to leave the
/// trailing partial token in the next chunk.
fn latest_whitespace_cut(text: &str) -> Option<usize> {
    text.char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
}

/// Round `cap` down to the nearest UTF-8 char boundary in `text`.
/// Used as the last-resort cut when neither a sentence boundary
/// nor whitespace exists in the buffer.
fn char_floor(text: &str, cap: usize) -> usize {
    let mut cap = cap.min(text.len());
    while cap > 0 && !text.is_char_boundary(cap) {
        cap -= 1;
    }
    cap
}

/// Position to cut at for an R2 paragraph-break release. Returns
/// the byte offset just past the chosen `\n\n`, with
/// [paragraph + list] grouping: when a list marker (`-`, `*`,
/// or `\d+\. `) follows the first break, look for the next `\n\n`
/// after the list. Returns `None` when no usable break exists.
fn paragraph_break_cut(text: &str) -> Option<usize> {
    let mut search_from = 0;
    loop {
        let suffix = text.get(search_from..)?;
        let rel = suffix.find("\n\n")?;
        let break_end = search_from + rel + 2; // position past "\n\n"
        let after = &text[break_end..];
        if starts_with_list_marker(after) {
            // Skip past this break \u2014 it's the para\u2192list transition.
            // Continue searching for the next \n\n past the list.
            search_from = break_end;
            continue;
        }
        return Some(break_end);
    }
}

/// `true` if `s` begins (after any leading spaces/tabs) with a
/// list marker recognized by R2: `- `, `* `, or `\d+\. `.
fn starts_with_list_marker(s: &str) -> bool {
    let s = s.trim_start_matches([' ', '\t']);
    if s.starts_with("- ") || s.starts_with("* ") {
        return true;
    }
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    i > 0
        && i + 1 < bytes.len()
        && bytes[i] == b'.'
        && (bytes[i + 1] == b' ' || bytes[i + 1] == b'\t')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default policy plus a few overrides for terser tests.
    fn policy() -> ChunkPolicy {
        ChunkPolicy::default()
    }

    /// Tiny helper to assert the next release is one chunk with
    /// the expected text and `final_for_turn` flag. Returns the
    /// chunk for further inspection.
    fn assert_one(out: Vec<ReleasedChunk>) -> ReleasedChunk {
        assert_eq!(out.len(), 1, "expected exactly one release, got {out:#?}");
        out.into_iter().next().unwrap()
    }

    // ---- R1: first-chunk fast path ---------------------------------

    #[test]
    fn r1_two_sentences_release_immediately() {
        let mut p = ChunkPlanner::new(policy());
        let out = p.push("Hello there. How are you? Extra.", 0);
        let chunk = assert_one(out);
        assert_eq!(chunk.index, 0);
        assert_eq!(chunk.text, "Hello there. How are you?");
        assert!(!chunk.final_for_turn);
    }

    #[test]
    fn r1_one_sentence_falls_back_after_max_wait() {
        let mut p = ChunkPlanner::new(policy());
        // First sentence arrives at t=0; second never arrives.
        let out = p.push("Hello there. ", 0);
        assert!(out.is_empty(), "should wait for second sentence");
        // Tick past the wait window.
        let out = p.tick(800);
        let chunk = assert_one(out);
        assert_eq!(chunk.text, "Hello there.");
        assert_eq!(chunk.index, 0);
    }

    #[test]
    fn r1_does_not_fall_back_before_wait_elapses() {
        let mut p = ChunkPlanner::new(policy());
        let out = p.push("Hello there. ", 0);
        assert!(out.is_empty());
        // Just under the threshold.
        let out = p.tick(799);
        assert!(out.is_empty(), "release should not fire yet");
    }

    // ---- R2: paragraph break ---------------------------------------

    #[test]
    fn r2_paragraph_release_at_double_newline() {
        let mut p = ChunkPlanner::new(policy());
        // Three sentences then a paragraph break.
        let out = p.push("S1. S2. S3.\n\nNext paragraph.", 0);
        // R1 (>=2 sentences) fires on the first push, taking S1+S2.
        // The remainder ("S3.\n\nNext paragraph.") stays buffered
        // until single-flight clears.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "S1. S2.");
        // Clear backpressure; R2 should now fire on the buffered
        // paragraph break.
        let out = p.synthesis_completed(0, 100);
        let chunk = assert_one(out);
        assert_eq!(chunk.text, "S3.");
        assert_eq!(chunk.index, 1);
    }

    #[test]
    fn r2_paragraph_with_list_groups_to_next_break() {
        let mut p = ChunkPlanner::new(policy());
        // Push the whole turn at once. R1 takes "S1. S2." first.
        let out = p.push(
            "S1. S2. Para intro:\n\n- item one\n- item two\n\nNext para.",
            0,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "S1. S2.");
        // Unblock; R2 should now group [paragraph + list] up to the
        // next \n\n.
        let out = p.synthesis_completed(0, 50);
        let chunk = assert_one(out);
        assert_eq!(chunk.text, "Para intro:\n\n- item one\n- item two");
    }

    // ---- R3: paragraph-wait timer ----------------------------------

    #[test]
    fn r3_releases_at_latest_sentence_after_paragraph_wait() {
        let mut p = ChunkPlanner::new(policy());
        // Three sentences arriving quickly, no paragraph break.
        // Continuation begins with a capital letter so the
        // abbreviation rule doesn't merge "S3." into "trailing".
        let out = p.push("S1. S2. S3. Trailing words", 0);
        // R1 takes S1+S2 immediately.
        assert_eq!(out.len(), 1);
        // Unblock so R3 can fire later.
        let out = p.synthesis_completed(0, 0);
        assert!(out.is_empty(), "no rule fires yet");
        // wait_started_at = 0 (set when "S3. Trailing words" remained
        // buffered after the cut). Tick past paragraph_wait_ms.
        let out = p.tick(3_000);
        let chunk = assert_one(out);
        assert_eq!(chunk.text, "S3.");
        assert_eq!(chunk.index, 1);
    }

    #[test]
    fn r3_grace_cuts_at_whitespace_when_no_sentence_boundary() {
        let mut p = ChunkPlanner::new(policy());
        // No sentence terminator at all.
        let out = p.push("just some words with no terminator", 0);
        assert!(out.is_empty());
        // paragraph_wait_ms passes \u2014 still no boundary, must wait grace.
        let out = p.tick(3_000);
        assert!(out.is_empty(), "grace not yet elapsed");
        // grace elapses too.
        let out = p.tick(4_000);
        let chunk = assert_one(out);
        // Expect text trimmed at last whitespace before "terminator".
        assert_eq!(chunk.text, "just some words with no");
    }

    // ---- R4: hard cap ----------------------------------------------

    #[test]
    fn r4_hard_cap_cuts_at_latest_whitespace_when_no_terminator() {
        let mut p = ChunkPlanner::new(policy());
        // 1500-byte run of "ab " repeated, no terminators.
        let mut text = String::new();
        while text.len() < 1_500 {
            text.push_str("ab ");
        }
        let out = p.push(&text, 0);
        let chunk = assert_one(out);
        // Cut should be at the latest whitespace in the buffer.
        assert!(
            chunk.text.ends_with("ab"),
            "expected to end on a token: {:?}",
            &chunk.text[chunk.text.len().saturating_sub(20)..]
        );
        // Some leftover should remain buffered (the trailing "ab ").
    }

    #[test]
    fn r4_hard_cap_cuts_at_sentence_when_available() {
        let mut p = ChunkPlanner::new(policy());
        // Build text ~1500 chars long that contains an early sentence
        // boundary; cap should still prefer the sentence boundary.
        // Continuation must start with a capital letter so the strict
        // abbreviation rule doesn't swallow "Short sentence one.".
        let mut text = String::from("Short sentence one. ");
        while text.len() < 1_500 {
            text.push_str("Filler ");
        }
        let out = p.push(&text, 0);
        // R1 fires on the visible 1+ sentences, hard cap path also
        // available; both prefer the same cut at "Short sentence one.".
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "Short sentence one.");
    }

    // ---- R5: idle timeout ------------------------------------------

    #[test]
    fn r5_idle_timeout_releases_buffered_sentence() {
        let mut p = ChunkPlanner::new(policy());
        // One sentence, then silence past idle_timeout_ms.
        let out = p.push("Hi there. ", 0);
        assert!(out.is_empty(), "R1 fast path needs 2 sentences");
        // R5 fires at idle_timeout_ms (1500), but R1 fallback fires
        // earlier (800) \u2014 verify R1 fires first as expected.
        let out = p.tick(800);
        let chunk = assert_one(out);
        assert_eq!(chunk.text, "Hi there.");
    }

    #[test]
    fn r5_idle_timeout_releases_after_first_chunk() {
        let mut p = ChunkPlanner::new(policy());
        // Force R1 to fire so subsequent R5 governs.
        let out = p.push("First. Second. Third", 0);
        assert_eq!(out.len(), 1);
        let _ = p.synthesis_completed(0, 0);
        // Push a sentence at t=100; idle threshold is now+1500 = 1600.
        let _ = p.push(" Fourth. ", 100);
        let out = p.tick(1_600);
        let chunk = assert_one(out);
        assert_eq!(chunk.text, "Third Fourth.");
    }

    // ---- R6: finish ------------------------------------------------

    #[test]
    fn finish_emits_remaining_text_with_final_flag() {
        let mut p = ChunkPlanner::new(policy());
        let _ = p.push("A. B. C", 0);
        // R1 takes "A. B."; "C" remains pending without terminator.
        let out = p.finish(50);
        let chunk = assert_one(out);
        assert_eq!(chunk.text, "C");
        assert!(chunk.final_for_turn);
    }

    #[test]
    fn finish_splits_remaining_paragraphs() {
        let mut p = ChunkPlanner::new(policy());
        // R1 will take the first 2 sentences; then we finish with
        // a multi-paragraph leftover.
        let _ = p.push("S1. S2. P1 body.\n\nP2 body.\n\nP3 body.", 0);
        let out = p.finish(0);
        // Expect 3 chunks: P1, P2, P3 (P3 final).
        assert_eq!(out.len(), 3, "{out:#?}");
        assert_eq!(out[0].text, "P1 body.");
        assert!(!out[0].final_for_turn);
        assert_eq!(out[1].text, "P2 body.");
        assert!(!out[1].final_for_turn);
        assert_eq!(out[2].text, "P3 body.");
        assert!(out[2].final_for_turn);
    }

    #[test]
    fn finish_is_idempotent() {
        let mut p = ChunkPlanner::new(policy());
        let _ = p.push("Lone sentence.", 0);
        let first = p.finish(0);
        assert_eq!(first.len(), 1);
        let second = p.finish(0);
        assert!(second.is_empty());
    }

    // ---- Single-flight ---------------------------------------------

    #[test]
    fn single_flight_blocks_subsequent_releases() {
        let mut p = ChunkPlanner::new(policy());
        // Push two paragraphs back-to-back. R1 takes the first
        // chunk; the second paragraph stays buffered until the
        // first synthesis completes.
        let out = p.push("P1 a. P1 b.\n\nP2 a. P2 b.\n\nP3.", 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "P1 a. P1 b.");
        // No tick / no completion \u2014 nothing else releases.
        let out = p.tick(0);
        assert!(out.is_empty(), "single-flight must hold subsequent chunks");
        // Completion unblocks; next chunk releases on the same call.
        let out = p.synthesis_completed(0, 10);
        let chunk = assert_one(out);
        assert_eq!(chunk.text, "P2 a. P2 b.");
        assert_eq!(chunk.index, 1);
    }

    // ---- Indices ---------------------------------------------------

    #[test]
    fn indices_increment_monotonically_from_zero() {
        let mut p = ChunkPlanner::new(policy());
        // R1 takes "S1. S2."; \n\n breaks then form successive R2
        // chunks; finish() flushes the final paragraph.
        let out = p.push("S1. S2.\n\nNext.\n\nThird.", 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "S1. S2.");
        assert_eq!(out[0].index, 0);
        let out = p.synthesis_completed(0, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "Next.");
        assert_eq!(out[0].index, 1);
        let out = p.finish(2);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "Third.");
        assert_eq!(out[0].index, 2);
        assert!(out[0].final_for_turn);
    }

    // ---- Misc edge cases -------------------------------------------

    #[test]
    fn empty_push_is_noop() {
        let mut p = ChunkPlanner::new(policy());
        assert!(p.push("", 0).is_empty());
        assert!(p.tick(0).is_empty());
    }

    #[test]
    fn tick_releases_when_text_already_buffered_and_idle() {
        // Verifies tick() (no new push) still consults timers \u2014
        // covers the orchestrator's polling behavior between LLM
        // emissions.
        let mut p = ChunkPlanner::new(policy());
        let _ = p.push("Lone sentence. ", 0);
        // R1 fallback at 800 \u2014 only timer that should fire.
        let early = p.tick(799);
        assert!(early.is_empty());
        let late = p.tick(800);
        assert_eq!(late.len(), 1);
        assert_eq!(late[0].text, "Lone sentence.");
    }

    #[test]
    fn paragraph_break_cut_skips_dash_list() {
        // White-box: verify the helper directly.
        let text = "Intro:\n\n- one\n- two\n\nAfter.";
        let cut = paragraph_break_cut(text).unwrap();
        // Expect cut at end of "...two\n\n", just before "After."
        assert_eq!(&text[..cut], "Intro:\n\n- one\n- two\n\n");
    }

    #[test]
    fn paragraph_break_cut_skips_numbered_list() {
        let text = "Steps:\n\n1. first\n2. second\n\nDone.";
        let cut = paragraph_break_cut(text).unwrap();
        assert_eq!(&text[..cut], "Steps:\n\n1. first\n2. second\n\n");
    }

    #[test]
    fn paragraph_break_cut_returns_none_when_list_unterminated() {
        // Spec acceptable behavior: no R2 cut yet; R3/R4 will handle.
        let text = "Intro:\n\n- one\n- two";
        assert!(paragraph_break_cut(text).is_none());
    }
}
