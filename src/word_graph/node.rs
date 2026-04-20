//! Nodes — atoms of the word graph.
//!
//! See [`docs/word-graph-spec.md`](../../../../docs/word-graph-spec.md) §1.

/// Index into the `WordGraph` node arena. Stable for the lifetime of the graph
/// (no compaction in the v1 slice).
pub type NodeId = u32;

/// What kind of element a node represents. Mutually exclusive (enum).
///
/// Spec §1.1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    /// A spoken or typed word.
    Word,
    /// Inline punctuation (`.`, `,`, `?`, `!`, `;`, `:`, `—`, quotes). Stored
    /// as a separate node, not attached to a preceding word, so it can carry
    /// its own provenance and confidence and be independently styled.
    Punctuation,
    /// A timed gap in speech. `text` is empty; duration lives in
    /// `start_ms`/`end_ms`.
    Silence,
    /// An explicit structural break. `text` is `"\n"` (line) or `"\n\n"`
    /// (paragraph).
    Break,
}

/// Where this node came from. Mutually exclusive (enum).
///
/// Spec §1.2 (with `AiGenerated` added for Conversation Mode).
///
/// `LlmFormatted` and `AiGenerated` are deliberately distinct:
/// - `LlmFormatted` — node originated as `Stt` (or another origin) and was
///   post-processed by an LLM formatter pass.
/// - `AiGenerated` — node was originally produced by an LLM as a turn in a
///   conversation, with no underlying audio source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeOrigin {
    /// Produced by the STT provider.
    Stt,
    /// Inserted or modified by the LLM formatting pass.
    LlmFormatted,
    /// Produced by an LLM as a conversational response (Conversation Mode).
    AiGenerated,
    /// Typed by the user.
    UserTyped,
}

/// Cross-cutting boolean properties. Combinable (bitfield).
///
/// Spec §1.3. Sixteen bits available; only the flags Conversation Mode v1
/// needs are defined here. Future flags (proper noun, technical term, bold,
/// italic, etc.) get added as features land.
pub type NodeFlags = u16;

/// Word is a filler (um, uh, er, ah, etc.). Set by a filler-detection pass;
/// rendered conditionally by projection filters.
pub const FLAG_FILLER: NodeFlags = 1 << 0;

/// Node belongs to an in-progress STT turn. While set, the UI renders the
/// node normally but blocks editing. Cleared when the turn finalizes
/// (`end_of_turn = true`).
pub const FLAG_TURN_LOCKED: NodeFlags = 1 << 1;

/// A graph node.
///
/// Spec §1.4. All nodes — words, punctuation, silence, breaks, across all
/// lanes and origins — share this struct. Distinguished by `kind` and
/// `origin`.
#[derive(Clone, Debug)]
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
    /// Word text, punctuation glyph, `"\n"` / `"\n\n"` for breaks, empty for
    /// silence.
    pub text: String,
    /// Recognition confidence 0.0–1.0 (from STT or synthetic).
    pub confidence: f32,
    /// Start time in milliseconds, relative to session start.
    pub start_ms: f64,
    /// End time in milliseconds, relative to session start.
    pub end_ms: f64,
    /// Lane index (0, 1, …). One lane per speaker in the session.
    pub speaker: u8,
    pub origin: NodeOrigin,
    pub flags: NodeFlags,
}

impl Node {
    pub fn is_filler(&self) -> bool {
        self.flags & FLAG_FILLER != 0
    }

    pub fn set_filler(&mut self) {
        self.flags |= FLAG_FILLER;
    }

    pub fn clear_filler(&mut self) {
        self.flags &= !FLAG_FILLER;
    }

    pub fn is_turn_locked(&self) -> bool {
        self.flags & FLAG_TURN_LOCKED != 0
    }

    pub fn set_turn_locked(&mut self) {
        self.flags |= FLAG_TURN_LOCKED;
    }

    pub fn clear_turn_locked(&mut self) {
        self.flags &= !FLAG_TURN_LOCKED;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_node() -> Node {
        Node {
            id: 0,
            kind: NodeKind::Word,
            text: "hello".to_string(),
            confidence: 0.9,
            start_ms: 0.0,
            end_ms: 100.0,
            speaker: 0,
            origin: NodeOrigin::Stt,
            flags: 0,
        }
    }

    /// Every NodeKind variant is constructible and equality-comparable.
    /// If a variant is added or removed, this test breaks.
    #[test]
    fn node_kind_all_variants_distinct() {
        let kinds = [
            NodeKind::Word,
            NodeKind::Punctuation,
            NodeKind::Silence,
            NodeKind::Break,
        ];
        for (i, a) in kinds.iter().enumerate() {
            for (j, b) in kinds.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b, "variant {i:?} must equal itself");
                } else {
                    assert_ne!(a, b, "variants {a:?} and {b:?} must be distinct");
                }
            }
        }
    }

    /// All four NodeOrigin variants exist and are distinct, including the
    /// new `AiGenerated` variant required by Conversation Mode.
    #[test]
    fn node_origin_all_variants_distinct_including_ai_generated() {
        let origins = [
            NodeOrigin::Stt,
            NodeOrigin::LlmFormatted,
            NodeOrigin::AiGenerated,
            NodeOrigin::UserTyped,
        ];
        for (i, a) in origins.iter().enumerate() {
            for (j, b) in origins.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "origins {a:?} and {b:?} must be distinct");
                }
            }
        }
    }

    /// Default flags zero state — node has no special properties.
    #[test]
    fn fresh_node_has_no_flags_set() {
        let n = fresh_node();
        assert_eq!(n.flags, 0);
        assert!(!n.is_filler());
        assert!(!n.is_turn_locked());
    }

    /// FLAG_FILLER is independent of FLAG_TURN_LOCKED. Setting one must not
    /// affect the other.
    #[test]
    fn flag_filler_independent_of_turn_locked() {
        let mut n = fresh_node();
        n.set_filler();
        assert!(n.is_filler());
        assert!(!n.is_turn_locked(), "turn-locked must remain unset");

        n.set_turn_locked();
        assert!(
            n.is_filler(),
            "filler must remain set when turn-locked is added"
        );
        assert!(n.is_turn_locked());

        n.clear_filler();
        assert!(!n.is_filler());
        assert!(
            n.is_turn_locked(),
            "turn-locked must remain set when filler is cleared"
        );
    }

    /// Setting an already-set flag is idempotent; clearing an unset flag is a no-op.
    #[test]
    fn flag_setters_are_idempotent() {
        let mut n = fresh_node();
        n.set_filler();
        n.set_filler();
        assert!(n.is_filler());
        assert_eq!(n.flags & FLAG_FILLER, FLAG_FILLER);

        n.clear_filler();
        n.clear_filler();
        assert!(!n.is_filler());
    }

    /// The two defined flag bits do not collide.
    #[test]
    fn defined_flag_bits_are_disjoint() {
        assert_eq!(FLAG_FILLER & FLAG_TURN_LOCKED, 0);
    }
}
