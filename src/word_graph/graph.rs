//! `WordGraph` — arena, adjacency, ingest, and queries.
//!
//! See [`docs/word-graph-spec.md`](../../../../docs/word-graph-spec.md) §3.
//!
//! ## Slice scope
//!
//! Implements the operations Conversation Mode v1 requires:
//! - `new`
//! - `ingest_turn` (STT input; per-lane spine; turn-lock semantics)
//! - `walk_spine`, `edges_from`, `edges_to`
//!
//! Editing operations (`replace_span`, `delete_span`, `insert_after`),
//! analysis passes (`analyze_temporal`, `reanalyze_range`), projection
//! filtering (`ProjectionOpts`, `project`, `project_interleaved`), and the
//! LLM exchange surface are deferred until the features that need them ship.

use std::collections::HashMap;

use super::edge::{Edge, EdgeKind};
use super::node::{FLAG_TURN_LOCKED, Node, NodeFlags, NodeId, NodeKind, NodeOrigin};

/// Input from an STT provider.
///
/// Spec §3.3. Maps directly to the AssemblyAI v3 `words` entry.
#[derive(Clone, Debug)]
pub struct SttWord {
    /// Word text, possibly with trailing punctuation attached
    /// (e.g., `"Hi,"`). `ingest_turn` splits trailing punctuation into
    /// separate `Punctuation` nodes.
    pub text: String,
    /// Word start time in milliseconds, relative to session start.
    pub start_ms: f64,
    /// Word end time in milliseconds, relative to session start.
    pub end_ms: f64,
    /// Recognition confidence 0.0–1.0.
    pub confidence: f32,
    /// `false` = the word may still change in subsequent partial-turn
    /// messages. `true` = the word is finalized for this turn.
    pub word_is_final: bool,
}

/// Punctuation glyphs that get split off the trailing edge of an STT word.
const TRAILING_PUNCT: &[char] = &['.', ',', '?', '!', ';', ':', '\'', '"'];

/// The runtime data model backing the transcript.
///
/// Spec §3.1.
///
/// ## Memory representation
///
/// - `nodes` — append-only arena. `NodeId` is an index into this vec. Stable
///   for the lifetime of the graph (no compaction in v1).
/// - `edges` — flat `Vec<Option<Edge>>`. `None` slots are tombstones for
///   edges that were removed during partial-turn updates. Tombstones are
///   filtered out by query methods. Compaction is deferred.
/// - `roots[speaker as usize]` — first node of each speaker's spine, or
///   `None` if that speaker has not yet spoken.
/// - `outgoing` / `incoming` — adjacency indices: `NodeId` → indices into
///   `edges`. Maintained on insert/remove.
///
/// ## Per-speaker turn state
///
/// - `finalized_tail[speaker]` — the last finalized node on this speaker's
///   spine, i.e., the node the next turn's first word should attach to.
/// - `active_turn[speaker]` — node IDs of the currently in-progress turn for
///   this speaker, all flagged `FLAG_TURN_LOCKED`. Cleared when the turn
///   finalizes.
pub struct WordGraph {
    nodes: Vec<Node>,
    edges: Vec<Option<Edge>>,
    roots: Vec<Option<NodeId>>,
    outgoing: HashMap<NodeId, Vec<usize>>,
    incoming: HashMap<NodeId, Vec<usize>>,
    finalized_tail: HashMap<u8, NodeId>,
    active_turn: HashMap<u8, Vec<NodeId>>,
}

impl Default for WordGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl WordGraph {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
            roots: Vec::new(),
            outgoing: HashMap::new(),
            incoming: HashMap::new(),
            finalized_tail: HashMap::new(),
            active_turn: HashMap::new(),
        }
    }

    /// Number of nodes in the arena (including any that have been orphaned
    /// during partial-turn replacement). Mostly useful for tests and metrics.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of live (non-tombstoned) edges.
    pub fn edge_count(&self) -> usize {
        self.edges.iter().filter(|e| e.is_some()).count()
    }

    /// Get a node by id. Returns `None` if `id` is out of range.
    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id as usize)
    }

    /// Root of `speaker`'s spine, or `None` if the speaker has no nodes yet.
    pub fn root(&self, speaker: u8) -> Option<NodeId> {
        self.roots.get(speaker as usize).copied().flatten()
    }

    /// Walk the primary spine (`Next` edges) for `speaker`, returning node
    /// IDs in order. Empty if the speaker has no spine.
    ///
    /// Spec §3.2.
    pub fn walk_spine(&self, speaker: u8) -> Vec<NodeId> {
        let Some(root) = self.root(speaker) else {
            return Vec::new();
        };
        let mut out = vec![root];
        let mut current = root;
        loop {
            // Find the next Next edge from `current`. By construction there
            // is at most one (per-lane spines do not branch in this slice).
            let next = self.outgoing.get(&current).and_then(|idxs| {
                idxs.iter().find_map(|&i| match &self.edges[i] {
                    Some(e) if e.kind == EdgeKind::Next => Some(e.to),
                    _ => None,
                })
            });
            match next {
                Some(n) => {
                    out.push(n);
                    current = n;
                }
                None => break,
            }
        }
        out
    }

    /// Outgoing edges of a specific kind from `node`.
    ///
    /// Spec §3.2.
    pub fn edges_from(&self, node: NodeId, kind: EdgeKind) -> Vec<&Edge> {
        self.outgoing
            .get(&node)
            .map(|idxs| {
                idxs.iter()
                    .filter_map(|&i| match &self.edges[i] {
                        Some(e) if e.kind == kind => Some(e),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Incoming edges of a specific kind to `node`.
    ///
    /// Spec §3.2.
    pub fn edges_to(&self, node: NodeId, kind: EdgeKind) -> Vec<&Edge> {
        self.incoming
            .get(&node)
            .map(|idxs| {
                idxs.iter()
                    .filter_map(|&i| match &self.edges[i] {
                        Some(e) if e.kind == kind => Some(e),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Add or update nodes from an STT turn event.
    ///
    /// Spec §3.2 + §3.3.
    ///
    /// Semantics:
    /// - **Partial turn** (`end_of_turn = false`): if the speaker already has
    ///   an in-progress turn, its nodes are orphaned (their `Next` edges are
    ///   tombstoned; the nodes remain in the arena) and the new word list
    ///   replaces them. New nodes are flagged `FLAG_TURN_LOCKED`.
    /// - **Final turn** (`end_of_turn = true`): same as partial, then the
    ///   in-progress turn is closed — `FLAG_TURN_LOCKED` is cleared on the
    ///   new nodes and `finalized_tail` advances to the last new node.
    ///
    /// Trailing punctuation in `SttWord.text` (e.g., `"Hi,"`) is split into
    /// a separate `Punctuation` node following the `Word`.
    ///
    /// **Note (slice limitation):** Filler detection at ingest is deferred;
    /// no `FLAG_FILLER` bits are set here. A future `detect_fillers` pass
    /// will set them.
    pub fn ingest_turn(&mut self, speaker: u8, words: &[SttWord], end_of_turn: bool) {
        // 1. Orphan any in-progress turn for this speaker.
        self.orphan_active_turn(speaker);

        // 2. Build new nodes for this turn.
        let lock_flag: NodeFlags = if end_of_turn { 0 } else { FLAG_TURN_LOCKED };
        let mut new_ids: Vec<NodeId> = Vec::with_capacity(words.len());
        for w in words {
            self.append_stt_word(speaker, w, lock_flag, &mut new_ids);
        }

        // 3. Wire Next edges among the new nodes.
        for window in new_ids.windows(2) {
            self.add_edge(window[0], window[1], EdgeKind::Next);
        }

        // 4. Attach the new turn to the lane.
        if let Some(first_new) = new_ids.first().copied() {
            match self.finalized_tail.get(&speaker).copied() {
                Some(tail) => {
                    self.add_edge(tail, first_new, EdgeKind::Next);
                }
                None => {
                    self.set_root(speaker, first_new);
                }
            }
        }

        // 5. Update per-speaker turn state.
        if end_of_turn {
            if let Some(last_new) = new_ids.last().copied() {
                self.finalized_tail.insert(speaker, last_new);
            }
            self.active_turn.remove(&speaker);
        } else if !new_ids.is_empty() {
            self.active_turn.insert(speaker, new_ids);
        } else {
            self.active_turn.remove(&speaker);
        }
    }

    // ── Internal helpers ────────────────────────────────────────────────

    fn next_node_id(&self) -> NodeId {
        self.nodes.len() as NodeId
    }

    fn push_node(&mut self, node: Node) -> NodeId {
        let id = node.id;
        debug_assert_eq!(
            id as usize,
            self.nodes.len(),
            "node id must equal arena index"
        );
        self.nodes.push(node);
        id
    }

    fn add_edge(&mut self, from: NodeId, to: NodeId, kind: EdgeKind) {
        let idx = self.edges.len();
        self.edges.push(Some(Edge { from, to, kind }));
        self.outgoing.entry(from).or_default().push(idx);
        self.incoming.entry(to).or_default().push(idx);
    }

    /// Tombstone the edge at `idx` and remove it from adjacency lists.
    fn drop_edge(&mut self, idx: usize) {
        let Some(edge) = self.edges[idx].take() else {
            return;
        };
        if let Some(list) = self.outgoing.get_mut(&edge.from) {
            list.retain(|&i| i != idx);
        }
        if let Some(list) = self.incoming.get_mut(&edge.to) {
            list.retain(|&i| i != idx);
        }
    }

    fn set_root(&mut self, speaker: u8, node: NodeId) {
        let i = speaker as usize;
        if self.roots.len() <= i {
            self.roots.resize(i + 1, None);
        }
        self.roots[i] = Some(node);
    }

    /// Drop all `Next` edges into and out of the speaker's in-progress turn,
    /// including the edge from `finalized_tail` (or the root) into the first
    /// active node. Nodes themselves remain in the arena.
    fn orphan_active_turn(&mut self, speaker: u8) {
        let Some(active) = self.active_turn.remove(&speaker) else {
            return;
        };
        let Some(first_active) = active.first().copied() else {
            return;
        };

        // Drop the inbound edge to the first active node (from finalized_tail
        // if any, else the lane root — which IS first_active itself when the
        // active turn was the lane's only content; in that case no inbound
        // edge exists, but we still need to clear the root).
        let inbound: Vec<usize> = self
            .incoming
            .get(&first_active)
            .cloned()
            .unwrap_or_default();
        for idx in inbound {
            if let Some(Some(e)) = self.edges.get(idx)
                && e.kind == EdgeKind::Next
            {
                self.drop_edge(idx);
            }
        }

        // If the lane root was the first orphaned node, clear it. The lane
        // is now in the same state as before the orphaned turn began.
        if self.root(speaker) == Some(first_active) {
            // Restore root to None — the next ingest will set it (or attach
            // to finalized_tail if any other turns had finalized first;
            // by construction in this slice, that cannot happen because
            // finalized_tail would be Some and we'd have an inbound edge).
            self.roots[speaker as usize] = None;
        }

        // Drop the Next edges chaining the active nodes together.
        for window in active.windows(2) {
            let from = window[0];
            let to = window[1];
            // Find the Next edge from `from` to `to` and drop it.
            let to_drop: Vec<usize> = self.outgoing.get(&from).cloned().unwrap_or_default();
            for idx in to_drop {
                if let Some(Some(e)) = self.edges.get(idx)
                    && e.kind == EdgeKind::Next
                    && e.to == to
                {
                    self.drop_edge(idx);
                }
            }
        }
    }

    /// Build node(s) for one `SttWord` and append them to `out_ids` in order.
    /// Splits trailing punctuation into discrete `Punctuation` nodes.
    fn append_stt_word(
        &mut self,
        speaker: u8,
        word: &SttWord,
        flags: NodeFlags,
        out_ids: &mut Vec<NodeId>,
    ) {
        // Split trailing punctuation.
        let mut text = word.text.as_str();
        let mut puncts: Vec<char> = Vec::new();
        while let Some(c) = text.chars().last() {
            if TRAILING_PUNCT.contains(&c) {
                puncts.push(c);
                text = &text[..text.len() - c.len_utf8()];
            } else {
                break;
            }
        }
        puncts.reverse();

        // Word node (unless the entire text was punctuation).
        if !text.is_empty() {
            let id = self.next_node_id();
            self.push_node(Node {
                id,
                kind: NodeKind::Word,
                text: text.to_string(),
                confidence: word.confidence,
                start_ms: word.start_ms,
                end_ms: word.end_ms,
                speaker,
                origin: NodeOrigin::Stt,
                flags,
            });
            out_ids.push(id);
        }

        // Punctuation nodes (one per trailing glyph).
        // No separate timing — inherit the word's end timing.
        for c in puncts {
            let id = self.next_node_id();
            let mut s = String::new();
            s.push(c);
            self.push_node(Node {
                id,
                kind: NodeKind::Punctuation,
                text: s,
                confidence: 1.0,
                start_ms: word.end_ms,
                end_ms: word.end_ms,
                speaker,
                origin: NodeOrigin::Stt,
                flags,
            });
            out_ids.push(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str, start_ms: f64, end_ms: f64) -> SttWord {
        SttWord {
            text: text.to_string(),
            start_ms,
            end_ms,
            confidence: 0.9,
            word_is_final: true,
        }
    }

    fn texts(g: &WordGraph, ids: &[NodeId]) -> Vec<String> {
        ids.iter()
            .map(|&id| g.node(id).expect("node exists").text.clone())
            .collect()
    }

    // ── Construction ────────────────────────────────────────────────────

    #[test]
    fn new_graph_is_empty() {
        let g = WordGraph::new();
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.edge_count(), 0);
        assert_eq!(g.root(0), None);
        assert!(g.walk_spine(0).is_empty());
    }

    // ── Single-turn ingest ──────────────────────────────────────────────

    #[test]
    fn ingest_final_turn_creates_root_and_spine() {
        let mut g = WordGraph::new();
        g.ingest_turn(0, &[w("Hello", 0.0, 200.0), w("world", 200.0, 500.0)], true);
        let spine = g.walk_spine(0);
        assert_eq!(texts(&g, &spine), vec!["Hello", "world"]);
        assert_eq!(g.root(0), Some(spine[0]));
        assert_eq!(g.edge_count(), 1, "one Next edge between two words");
    }

    #[test]
    fn ingest_final_turn_finalized_nodes_are_not_turn_locked() {
        let mut g = WordGraph::new();
        g.ingest_turn(0, &[w("ok", 0.0, 100.0)], true);
        let id = g.walk_spine(0)[0];
        assert!(!g.node(id).unwrap().is_turn_locked());
    }

    #[test]
    fn ingest_partial_turn_marks_nodes_turn_locked() {
        let mut g = WordGraph::new();
        g.ingest_turn(0, &[w("partial", 0.0, 100.0)], false);
        let id = g.walk_spine(0)[0];
        assert!(g.node(id).unwrap().is_turn_locked());
    }

    // ── Punctuation splitting ───────────────────────────────────────────

    #[test]
    fn ingest_splits_trailing_comma_into_punctuation_node() {
        let mut g = WordGraph::new();
        g.ingest_turn(0, &[w("Hi,", 0.0, 100.0), w("world", 100.0, 300.0)], true);
        let spine = g.walk_spine(0);
        assert_eq!(texts(&g, &spine), vec!["Hi", ",", "world"]);
        assert_eq!(g.node(spine[0]).unwrap().kind, NodeKind::Word);
        assert_eq!(g.node(spine[1]).unwrap().kind, NodeKind::Punctuation);
        assert_eq!(g.node(spine[2]).unwrap().kind, NodeKind::Word);
    }

    #[test]
    fn ingest_splits_multiple_trailing_punctuation() {
        let mut g = WordGraph::new();
        g.ingest_turn(0, &[w("Wait...", 0.0, 100.0)], true);
        let spine = g.walk_spine(0);
        assert_eq!(texts(&g, &spine), vec!["Wait", ".", ".", "."]);
        for &id in &spine[1..] {
            assert_eq!(g.node(id).unwrap().kind, NodeKind::Punctuation);
        }
    }

    #[test]
    fn ingest_pure_punctuation_word_emits_only_punctuation_nodes() {
        let mut g = WordGraph::new();
        g.ingest_turn(0, &[w("?", 0.0, 100.0)], true);
        let spine = g.walk_spine(0);
        assert_eq!(texts(&g, &spine), vec!["?"]);
        assert_eq!(g.node(spine[0]).unwrap().kind, NodeKind::Punctuation);
    }

    #[test]
    fn punctuation_nodes_inherit_word_end_timing() {
        let mut g = WordGraph::new();
        g.ingest_turn(0, &[w("Hi,", 100.0, 350.0)], true);
        let spine = g.walk_spine(0);
        let punct = g.node(spine[1]).unwrap();
        assert_eq!(punct.start_ms, 350.0);
        assert_eq!(punct.end_ms, 350.0);
    }

    // ── Multi-turn append ───────────────────────────────────────────────

    #[test]
    fn second_final_turn_appends_to_finalized_spine() {
        let mut g = WordGraph::new();
        g.ingest_turn(0, &[w("First", 0.0, 100.0)], true);
        g.ingest_turn(0, &[w("Second", 200.0, 400.0)], true);
        let spine = g.walk_spine(0);
        assert_eq!(texts(&g, &spine), vec!["First", "Second"]);
    }

    // ── Partial-turn replacement ────────────────────────────────────────

    #[test]
    fn partial_turn_followed_by_partial_replaces_active_nodes() {
        let mut g = WordGraph::new();
        g.ingest_turn(0, &[w("hel", 0.0, 50.0)], false);
        g.ingest_turn(0, &[w("hello", 0.0, 100.0)], false);
        let spine = g.walk_spine(0);
        assert_eq!(texts(&g, &spine), vec!["hello"]);
        // The "hel" node is orphaned but still in the arena.
        assert!(
            g.node_count() >= 2,
            "orphaned partial-turn nodes remain in the arena"
        );
    }

    #[test]
    fn partial_turn_then_final_turn_finalizes_nodes() {
        let mut g = WordGraph::new();
        g.ingest_turn(0, &[w("hel", 0.0, 50.0)], false);
        g.ingest_turn(0, &[w("hello", 0.0, 100.0)], true);
        let spine = g.walk_spine(0);
        let id = spine[0];
        assert_eq!(g.node(id).unwrap().text, "hello");
        assert!(!g.node(id).unwrap().is_turn_locked());
    }

    #[test]
    fn final_then_partial_turn_attaches_partial_after_finalized_tail() {
        let mut g = WordGraph::new();
        g.ingest_turn(0, &[w("Done.", 0.0, 200.0)], true);
        g.ingest_turn(0, &[w("nex", 300.0, 400.0)], false);
        let spine = g.walk_spine(0);
        assert_eq!(texts(&g, &spine), vec!["Done", ".", "nex"]);
        assert!(
            g.node(spine[2]).unwrap().is_turn_locked(),
            "in-progress turn nodes are turn-locked"
        );
        assert!(
            !g.node(spine[0]).unwrap().is_turn_locked(),
            "previously finalized nodes stay finalized"
        );
    }

    #[test]
    fn partial_replacement_does_not_affect_finalized_spine() {
        let mut g = WordGraph::new();
        g.ingest_turn(0, &[w("Done.", 0.0, 200.0)], true);
        g.ingest_turn(0, &[w("first", 300.0, 400.0)], false);
        g.ingest_turn(0, &[w("second", 300.0, 500.0)], false);
        let spine = g.walk_spine(0);
        assert_eq!(texts(&g, &spine), vec!["Done", ".", "second"]);
    }

    // ── Multi-lane forest ───────────────────────────────────────────────

    #[test]
    fn two_speakers_have_independent_spines() {
        let mut g = WordGraph::new();
        g.ingest_turn(0, &[w("Hello", 0.0, 200.0)], true);
        g.ingest_turn(1, &[w("Hi", 100.0, 300.0)], true);
        assert_eq!(texts(&g, &g.walk_spine(0)), vec!["Hello"]);
        assert_eq!(texts(&g, &g.walk_spine(1)), vec!["Hi"]);
        assert_ne!(g.root(0), g.root(1));
    }

    #[test]
    fn lanes_with_sparse_speaker_indices_are_supported() {
        // Speaker 5 speaks first; speaker 0 has never spoken.
        let mut g = WordGraph::new();
        g.ingest_turn(5, &[w("ping", 0.0, 100.0)], true);
        assert_eq!(g.root(0), None);
        assert_eq!(texts(&g, &g.walk_spine(5)), vec!["ping"]);
    }

    #[test]
    fn partial_turn_on_one_speaker_does_not_affect_another() {
        let mut g = WordGraph::new();
        g.ingest_turn(0, &[w("hel", 0.0, 50.0)], false);
        g.ingest_turn(1, &[w("ok", 0.0, 100.0)], true);
        // Replace speaker 0's partial.
        g.ingest_turn(0, &[w("hello", 0.0, 100.0)], false);
        assert_eq!(texts(&g, &g.walk_spine(1)), vec!["ok"]);
        assert!(!g.node(g.walk_spine(1)[0]).unwrap().is_turn_locked());
    }

    // ── Edge queries ────────────────────────────────────────────────────

    #[test]
    fn edges_from_returns_only_matching_kind() {
        let mut g = WordGraph::new();
        g.ingest_turn(0, &[w("a", 0.0, 50.0), w("b", 50.0, 100.0)], true);
        let root = g.root(0).unwrap();
        let next_edges = g.edges_from(root, EdgeKind::Next);
        assert_eq!(next_edges.len(), 1);
        assert_eq!(g.edges_from(root, EdgeKind::Alt).len(), 0);
        assert_eq!(g.edges_from(root, EdgeKind::Correction).len(), 0);
        assert_eq!(g.edges_from(root, EdgeKind::Temporal).len(), 0);
    }

    #[test]
    fn edges_to_returns_only_matching_kind() {
        let mut g = WordGraph::new();
        g.ingest_turn(0, &[w("a", 0.0, 50.0), w("b", 50.0, 100.0)], true);
        let spine = g.walk_spine(0);
        let second = spine[1];
        let in_edges = g.edges_to(second, EdgeKind::Next);
        assert_eq!(in_edges.len(), 1);
        assert_eq!(in_edges[0].from, spine[0]);
    }

    #[test]
    fn edges_from_unknown_node_returns_empty() {
        let g = WordGraph::new();
        assert!(g.edges_from(999, EdgeKind::Next).is_empty());
        assert!(g.edges_to(999, EdgeKind::Next).is_empty());
    }

    // ── Provenance ──────────────────────────────────────────────────────

    #[test]
    fn ingested_nodes_carry_stt_origin() {
        let mut g = WordGraph::new();
        g.ingest_turn(0, &[w("hi", 0.0, 100.0)], true);
        for id in g.walk_spine(0) {
            assert_eq!(g.node(id).unwrap().origin, NodeOrigin::Stt);
        }
    }

    #[test]
    fn ingested_nodes_carry_speaker_index() {
        let mut g = WordGraph::new();
        g.ingest_turn(7, &[w("hi,", 0.0, 100.0)], true);
        for id in g.walk_spine(7) {
            assert_eq!(g.node(id).unwrap().speaker, 7);
        }
    }

    // ── Edge cases ──────────────────────────────────────────────────────

    #[test]
    fn ingest_empty_words_on_fresh_lane_is_a_noop() {
        let mut g = WordGraph::new();
        g.ingest_turn(0, &[], true);
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.root(0), None);
    }

    #[test]
    fn ingest_empty_partial_turn_orphans_active_turn() {
        let mut g = WordGraph::new();
        g.ingest_turn(0, &[w("hel", 0.0, 50.0)], false);
        g.ingest_turn(0, &[], false);
        assert!(g.walk_spine(0).is_empty());
        assert_eq!(g.root(0), None);
    }

    #[test]
    fn ingest_unicode_text_preserved() {
        let mut g = WordGraph::new();
        g.ingest_turn(0, &[w("café", 0.0, 100.0)], true);
        let id = g.walk_spine(0)[0];
        assert_eq!(g.node(id).unwrap().text, "café");
    }

    #[test]
    fn walk_spine_returns_empty_for_unknown_speaker() {
        let mut g = WordGraph::new();
        g.ingest_turn(0, &[w("hi", 0.0, 100.0)], true);
        assert!(g.walk_spine(99).is_empty());
    }
}
