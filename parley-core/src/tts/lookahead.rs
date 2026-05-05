//! Paragraph lookahead feeder for turn-level streaming TTS.
//!
//! [`ParagraphLookaheadFeeder`] sits between raw LLM text deltas and
//! providers that accept incremental text for one continuous synthesis
//! session. It releases complete text units large enough to avoid
//! underfed prosody, while preserving paragraph boundaries inside the
//! same provider session.

use serde::{Deserialize, Serialize};

use crate::tts::sentence::{SentenceBoundary, find_all_boundaries_relaxed};

/// Policy for [`ParagraphLookaheadFeeder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LookaheadPolicy {
    /// Minimum characters for releasing a single completed paragraph.
    /// Shorter paragraphs are held for the next paragraph so providers
    /// such as xAI do not restart delivery from tiny fragments.
    pub min_release_chars: usize,
    /// Minimum sentence count for releasing a single completed
    /// paragraph even when it is shorter than [`Self::min_release_chars`].
    pub min_release_sentences: u32,
    /// Maximum number of completed short paragraphs to coalesce before
    /// releasing anyway.
    pub max_coalesced_paragraphs: u32,
    /// Safety cap for pending text. If reached, the feeder releases at
    /// the latest sentence boundary, then whitespace, then the cap.
    pub hard_cap_chars: usize,
}

impl Default for LookaheadPolicy {
    fn default() -> Self {
        Self {
            min_release_chars: 240,
            min_release_sentences: 2,
            max_coalesced_paragraphs: 2,
            hard_cap_chars: 1_500,
        }
    }
}

/// Buffers LLM text until it is safe to feed a turn-level streaming
/// TTS provider.
#[derive(Debug, Clone)]
pub struct ParagraphLookaheadFeeder {
    policy: LookaheadPolicy,
    pending: String,
}

impl ParagraphLookaheadFeeder {
    /// Build a feeder with `policy`.
    pub fn new(policy: LookaheadPolicy) -> Self {
        Self {
            policy,
            pending: String::new(),
        }
    }

    /// Push raw LLM text and return any text deltas now ready for the
    /// provider. Returned deltas preserve paragraph separators.
    pub fn push(&mut self, text: &str) -> Vec<String> {
        if text.is_empty() {
            return Vec::new();
        }
        self.pending.push_str(text);
        self.drain_ready()
    }

    /// Drain all remaining text at turn end.
    pub fn finish(&mut self) -> Vec<String> {
        if self.pending.trim().is_empty() {
            self.pending.clear();
            Vec::new()
        } else {
            let text = self.pending.trim_start().to_string();
            self.pending.clear();
            vec![text]
        }
    }

    fn drain_ready(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(cut) = self.next_cut() {
            let text = self.release(cut);
            if !text.trim().is_empty() {
                out.push(text);
            }
        }
        out
    }

    fn next_cut(&self) -> Option<usize> {
        if self.pending.len() >= self.policy.hard_cap_chars {
            return Some(
                latest_sentence_cut(&self.pending)
                    .or_else(|| latest_whitespace_cut(&self.pending))
                    .unwrap_or_else(|| char_floor(&self.pending, self.policy.hard_cap_chars)),
            );
        }

        let breaks = paragraph_breaks(&self.pending);
        let first_break = breaks.first().copied()?;
        let first = &self.pending[..first_break];
        if is_substantial(first, self.policy) {
            return Some(first_break);
        }

        let max_group = self.policy.max_coalesced_paragraphs.max(1) as usize;
        if breaks.len() >= max_group {
            return Some(breaks[max_group - 1]);
        }
        None
    }

    fn release(&mut self, cut: usize) -> String {
        let mut cut = cut.min(self.pending.len());
        while cut > 0 && !self.pending.is_char_boundary(cut) {
            cut -= 1;
        }
        let mut text = self.pending[..cut].trim_start().to_string();
        if !text.ends_with("\n\n") {
            text = text.trim_end().to_string();
        }
        self.pending.drain(..cut);
        let leading = self.pending.len() - self.pending.trim_start().len();
        if leading > 0 {
            self.pending.drain(..leading);
        }
        text
    }
}

fn paragraph_breaks(text: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut offset = 0;
    while let Some(rel) = text[offset..].find("\n\n") {
        offset += rel + 2;
        out.push(offset);
    }
    out
}

fn is_substantial(text: &str, policy: LookaheadPolicy) -> bool {
    text.trim().chars().count() >= policy.min_release_chars
        || find_all_boundaries_relaxed(text).len() as u32 >= policy.min_release_sentences
}

fn latest_sentence_cut(text: &str) -> Option<usize> {
    find_all_boundaries_relaxed(text)
        .last()
        .map(|boundary: &SentenceBoundary| boundary.consume_through)
}

fn latest_whitespace_cut(text: &str) -> Option<usize> {
    text.char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map(|(idx, c)| idx + c.len_utf8())
}

fn char_floor(text: &str, cap: usize) -> usize {
    let mut cap = cap.min(text.len());
    while cap > 0 && !text.is_char_boundary(cap) {
        cap -= 1;
    }
    cap
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feeder() -> ParagraphLookaheadFeeder {
        ParagraphLookaheadFeeder::new(LookaheadPolicy {
            min_release_chars: 40,
            min_release_sentences: 2,
            max_coalesced_paragraphs: 2,
            hard_cap_chars: 120,
        })
    }

    #[test]
    fn substantial_paragraph_releases_at_break() {
        let mut feeder = feeder();
        let out = feeder.push("This is one sentence. This is another.\n\nNext");
        assert_eq!(out, vec!["This is one sentence. This is another.\n\n"]);
        assert_eq!(feeder.finish(), vec!["Next"]);
    }

    #[test]
    fn single_short_paragraph_waits_for_more_context() {
        let mut feeder = feeder();
        let out = feeder.push("Right.\n\nNext paragraph is still coming");
        assert!(out.is_empty());
    }

    #[test]
    fn two_short_paragraphs_coalesce() {
        let mut feeder = feeder();
        let out = feeder.push("Right.\n\nExactly.\n\nNow the body starts.");
        assert_eq!(out, vec!["Right.\n\nExactly.\n\n"]);
        assert_eq!(feeder.finish(), vec!["Now the body starts."]);
    }

    #[test]
    fn hard_cap_releases_at_latest_sentence() {
        let mut feeder = feeder();
        let out = feeder.push(
            "This first sentence is intentionally long enough. This second sentence is also long enough. Trailing words without break",
        );
        assert_eq!(
            out,
            vec![
                "This first sentence is intentionally long enough. This second sentence is also long enough."
            ]
        );
    }

    #[test]
    fn finish_drains_unterminated_text() {
        let mut feeder = feeder();
        assert!(feeder.push("No paragraph break yet").is_empty());
        assert_eq!(feeder.finish(), vec!["No paragraph break yet"]);
        assert!(feeder.finish().is_empty());
    }
}
