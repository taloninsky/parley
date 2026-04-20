//! Edges — relationships between nodes.
//!
//! See [`docs/word-graph-spec.md`](../../../../docs/word-graph-spec.md) §2.
//!
//! ## Slice scope
//!
//! Only `EdgeKind::Next` is used in the Conversation Mode v1 prerequisite
//! slice. The `Alt`, `Correction`, and `Temporal` variants are deferred until
//! the features that need them ship.

use super::node::NodeId;

/// What relationship an edge represents. Mutually exclusive (enum).
///
/// Spec §2.1.
///
/// **v1 slice:** Only `Next` is constructed and queried by the rest of the
/// codebase. The other variants are defined here so adding them later does
/// not require an enum-shape change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeKind {
    /// Primary spine within a single speaker lane. Walking `Next` from a
    /// lane root yields the speaker's transcript in order.
    Next,
    /// Alternative transcription (lower-confidence branch). **Deferred.**
    Alt,
    /// Edit history: old node → its replacement. **Deferred.**
    Correction,
    /// Cross-speaker timing link. Derived; rebuildable. **Deferred.**
    Temporal,
}

impl EdgeKind {
    /// Whether this edge kind is computed from other data (`Temporal`) and
    /// can be safely cleared and recomputed, vs. intrinsic structure
    /// (`Next`, `Alt`, `Correction`).
    ///
    /// Spec §2.2.
    pub fn is_derived(self) -> bool {
        matches!(self, EdgeKind::Temporal)
    }
}

/// A directed edge between two nodes.
///
/// Spec §2.3. All edges share this struct regardless of kind; live in a
/// single flat `Vec<Edge>` on `WordGraph`.
#[derive(Clone, Debug)]
pub struct Edge {
    pub from: NodeId,
    pub to: NodeId,
    pub kind: EdgeKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant is distinct.
    #[test]
    fn edge_kind_all_variants_distinct() {
        let kinds = [
            EdgeKind::Next,
            EdgeKind::Alt,
            EdgeKind::Correction,
            EdgeKind::Temporal,
        ];
        for (i, a) in kinds.iter().enumerate() {
            for (j, b) in kinds.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "{a:?} and {b:?} must be distinct");
                }
            }
        }
    }

    /// `Temporal` is the only derived edge kind in v1. If a new derived
    /// kind is added later, this test must be updated to reflect that.
    #[test]
    fn only_temporal_is_derived() {
        assert!(!EdgeKind::Next.is_derived());
        assert!(!EdgeKind::Alt.is_derived());
        assert!(!EdgeKind::Correction.is_derived());
        assert!(EdgeKind::Temporal.is_derived());
    }

    /// Edge struct round-trips its fields.
    #[test]
    fn edge_constructs_with_expected_fields() {
        let e = Edge {
            from: 7,
            to: 11,
            kind: EdgeKind::Next,
        };
        assert_eq!(e.from, 7);
        assert_eq!(e.to, 11);
        assert_eq!(e.kind, EdgeKind::Next);
    }
}
