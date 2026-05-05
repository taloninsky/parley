//! Provider-neutral expression annotation vocabulary.
//!
//! Spec: `docs/conversation-mode-spec.md` §6.4. The LLM is taught
//! this vocabulary via an instruction the orchestrator auto-prepends
//! to the persona system prompt. Each `TtsProvider` translates the
//! neutral tags into its native syntax at synthesis time, so personas
//! stay portable across providers. The active provider owns the exact
//! prompt text shown to the LLM because each TTS model supports a
//! different expression surface.
//!
//! This module lives in `parley-core` so both the orchestrator
//! (auto-prepend) and the future browser-side stripping (display
//! formatter) share one vocabulary.

/// One neutral tag with the description shown to the LLM.
pub struct NeutralTag {
    /// Tag id, no braces (e.g. `"warm"`, `"pause:short"`). The
    /// matcher recognizes `{<id>}` in LLM output.
    pub id: &'static str,
    /// One-line description handed to the LLM in the auto-prepended
    /// instruction. Kept terse so the instruction stays cheap.
    pub description: &'static str,
}

/// The v1 neutral vocabulary. Conversational range only; theatrical
/// extremes (whisper/shout) are intentionally excluded per spec
/// decision D26.
pub const NEUTRAL_TAGS: &[NeutralTag] = &[
    NeutralTag {
        id: "warm",
        description: "Kind, gentle delivery.",
    },
    NeutralTag {
        id: "empathetic",
        description: "Feeling-with the listener.",
    },
    NeutralTag {
        id: "concerned",
        description: "Worried, careful tone.",
    },
    NeutralTag {
        id: "questioning",
        description: "Rising intonation, genuine inquiry.",
    },
    NeutralTag {
        id: "thoughtful",
        description: "Slower, considered, thinking out loud.",
    },
    NeutralTag {
        id: "excited",
        description: "Energized, animated.",
    },
    NeutralTag {
        id: "amused",
        description: "Light, mildly playful.",
    },
    NeutralTag {
        id: "laugh",
        description: "Actual short laugh sound.",
    },
    NeutralTag {
        id: "sarcastic",
        description: "Dry, knowing inflection.",
    },
    NeutralTag {
        id: "confused",
        description: "Uncertain, puzzled.",
    },
    NeutralTag {
        id: "sad",
        description: "Somber, low energy.",
    },
    NeutralTag {
        id: "soft",
        description: "Quieter, intimate (conversational range).",
    },
    NeutralTag {
        id: "emphasis",
        description: "Stress on a word or short phrase.",
    },
    NeutralTag {
        id: "sigh",
        description: "Short audible exhale. Use sparingly.",
    },
    NeutralTag {
        id: "pause:short",
        description: "Deliberate beat (~250ms).",
    },
    NeutralTag {
        id: "pause:medium",
        description: "Deliberate beat (~700ms).",
    },
    NeutralTag {
        id: "pause:long",
        description: "Deliberate beat (~1.5s).",
    },
];

/// Build the canonical provider-neutral instruction for this vocabulary.
///
/// The orchestrator now asks the active TTS provider for its own
/// expression instruction, because not every model supports every tag
/// or scoping rule. Providers can still use this helper when their
/// native expression surface matches the full neutral vocabulary.
///
/// Kept as a standalone function (not a `const`) so we can format
/// the tag list dynamically; the alternative would be a long
/// hand-maintained string.
pub fn expression_tag_instruction() -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(
        "You may annotate spoken responses with these expression tags inline. \
         Use them sparingly and only when they enhance meaning — most sentences \
         should carry no tags at all. Place each tag in its own pair of braces \
         exactly where the cue should land in the audio.\n\n\
         Available tags:\n",
    );
    for tag in NEUTRAL_TAGS {
        out.push_str("- {");
        out.push_str(tag.id);
        out.push_str("} — ");
        out.push_str(tag.description);
        out.push('\n');
    }
    out.push_str(
        "\nExample: \"That's a great question. {pause:short} {thoughtful} \
         Let me think about it.\"\n\n\
         Do not invent new tags. Do not nest tags. Tags appear inline as plain \
         text alongside the words they accompany; nothing else changes about \
         your reply.",
    );
    out
}

/// Strip every `{tag}` occurrence from text — used by display
/// formatters that don't want to surface the markers and by the
/// default `TtsProvider::translate_expression_tags` impl when a
/// provider can't render any of them.
///
/// Matches the same `{<id>}` shape as the LLM is taught: a literal
/// `{`, ASCII letters / digits / `:` / `_` / `-`, then `}`. Anything
/// else (e.g. JSON literals like `{"key":1}`) is left alone.
///
/// Whitespace is collapsed around dropped tags so the result reads
/// naturally: leading tags eat the trailing space, trailing tags
/// eat the leading space, and tags between two spaces collapse to
/// one space ("word {tag} word" → "word word").
pub fn strip_neutral_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(end) = find_tag_end(bytes, i + 1)
        {
            let before_space = out.ends_with(' ') || out.ends_with('\t');
            let out_was_empty = out.is_empty();
            let after_idx = end + 1;
            let after_space = bytes
                .get(after_idx)
                .map(|b| *b == b' ' || *b == b'\t')
                .unwrap_or(false);
            i = after_idx;
            // Decide which (if any) of the surrounding whitespace
            // chars to absorb. Goal: never leave a doubled space and
            // never strand a leading/trailing space at the very edge
            // of the result.
            if before_space && after_space {
                // word [SPACE] {tag} [SPACE] word → "word word".
                i += 1;
            } else if out_was_empty && after_space {
                // {tag} [SPACE] word → "word".
                i += 1;
            } else if before_space && i >= bytes.len() {
                // word [SPACE] {tag} <end> → "word".
                while out.ends_with(' ') || out.ends_with('\t') {
                    out.pop();
                }
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Find the closing `}` for a candidate tag starting at `start` (the
/// index just after the opening `{`). Returns `Some(close_idx)` only
/// when the bytes between `start..close_idx` look like a tag id
/// (letters, digits, `:`, `_`, `-`, non-empty); otherwise `None`,
/// which signals "not a tag, keep the `{` literal."
fn find_tag_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut j = start;
    while j < bytes.len() {
        let b = bytes[j];
        match b {
            b'}' => {
                if j == start {
                    return None;
                }
                return Some(j);
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b':' | b'_' | b'-' => j += 1,
            _ => return None,
        }
    }
    None
}

/// Iterator-yielding helper used by per-provider translators:
/// emits `(literal, Option<tag>)` pairs walking left to right. The
/// final pair has `tag = None` and `literal` is whatever followed
/// the last tag (possibly empty).
///
/// Returning slices instead of owned strings avoids one allocation
/// per pair on the hot path.
pub fn split_into_segments(text: &str) -> Vec<Segment<'_>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut cursor = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = find_tag_end(bytes, i + 1) {
                if cursor < i {
                    out.push(Segment::Text(&text[cursor..i]));
                }
                let tag = &text[i + 1..end];
                out.push(Segment::Tag(tag));
                i = end + 1;
                cursor = i;
                continue;
            }
        }
        i += 1;
    }
    if cursor < bytes.len() {
        out.push(Segment::Text(&text[cursor..]));
    }
    out
}

/// Output of [`split_into_segments`].
#[derive(Debug, PartialEq, Eq)]
pub enum Segment<'a> {
    /// Literal text between (or around) tag markers.
    Text(&'a str),
    /// A tag id without braces (e.g. `"warm"`, `"pause:short"`).
    Tag(&'a str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruction_contains_all_tags_and_example() {
        let s = expression_tag_instruction();
        for tag in NEUTRAL_TAGS {
            let marker = format!("{{{}}}", tag.id);
            assert!(
                s.contains(&marker),
                "instruction should mention {{{}}}: {s}",
                tag.id,
            );
        }
        assert!(s.contains("Example:"));
        assert!(s.contains("pause:short"));
    }

    #[test]
    fn strip_removes_simple_tag() {
        assert_eq!(strip_neutral_tags("{warm} hello"), "hello");
    }

    #[test]
    fn strip_collapses_surrounding_spaces() {
        assert_eq!(strip_neutral_tags("hi {warm} there"), "hi there");
    }

    #[test]
    fn strip_eats_trailing_space_for_leading_tag() {
        assert_eq!(strip_neutral_tags("{laugh} hello"), "hello");
    }

    #[test]
    fn strip_eats_leading_space_for_trailing_tag() {
        assert_eq!(strip_neutral_tags("hello {laugh}"), "hello");
    }

    #[test]
    fn strip_handles_pause_tag_with_colon() {
        assert_eq!(strip_neutral_tags("ok {pause:short} done"), "ok done",);
    }

    #[test]
    fn strip_leaves_json_braces_alone() {
        assert_eq!(
            strip_neutral_tags(r#"config: {"k":"v"}"#),
            r#"config: {"k":"v"}"#,
        );
    }

    #[test]
    fn strip_leaves_empty_braces_alone() {
        assert_eq!(strip_neutral_tags("nothing {} here"), "nothing {} here");
    }

    #[test]
    fn split_segments_in_order() {
        let segs = split_into_segments("hi {warm} there {pause:short}!");
        assert_eq!(
            segs,
            vec![
                Segment::Text("hi "),
                Segment::Tag("warm"),
                Segment::Text(" there "),
                Segment::Tag("pause:short"),
                Segment::Text("!"),
            ],
        );
    }

    #[test]
    fn split_handles_no_tags() {
        let segs = split_into_segments("plain text");
        assert_eq!(segs, vec![Segment::Text("plain text")]);
    }

    #[test]
    fn split_ignores_malformed_braces() {
        let segs = split_into_segments(r#"{"json":"x"} {tag}"#);
        assert_eq!(
            segs,
            vec![Segment::Text(r#"{"json":"x"} "#), Segment::Tag("tag"),],
        );
    }
}
