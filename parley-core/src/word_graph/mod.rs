//! Word Graph — runtime data model for the annotated stream.
//!
//! This module is the **minimal slice** of the word graph required before
//! Conversation Mode v1 can be implemented. Full specification:
//! [`docs/word-graph-spec.md`](../../../docs/word-graph-spec.md).
//! Slice scope and rationale: [`docs/conversation-mode-spec.md` §1.5](../../../docs/conversation-mode-spec.md).

// The slice intentionally exposes API surface (re-exports, helper methods,
// fields like `confidence` and `word_is_final`) that has no consumers yet —
// the STT pipeline migration that will use them is the next phase of work.
// These warnings will go away as the migration lands.
#![allow(dead_code, unused_imports)]
//! ## In Scope (this slice)
//!
//! - `NodeKind`, `NodeOrigin`, `NodeFlags`, `Node` (spec §1.1–§1.4)
//! - `EdgeKind` — **`Next` variant only** (spec §2.1)
//! - `Edge` (spec §2.3)
//! - `WordGraph` arena + adjacency, multi-lane forest (spec §3.1)
//! - Core operations: `new`, `ingest_turn`, `walk_spine`, `edges_from`, `edges_to` (subset of §3.2)
//! - `SttWord` input (spec §3.3)
//!
//! ## Deferred (NOT in this slice)
//!
//! - `EdgeKind::Alt`, `Correction`, `Temporal`
//! - `replace_span`, `delete_span`, `insert_after`, `analyze_temporal`,
//!   `reanalyze_range`, `to_llm_exchange`, `apply_llm_exchange`
//! - `ProjectionOpts` filters (spec §3.4)
//! - All of spec §4 (non-destructive editing)
//!
//! ## Design notes
//!
//! - Pure data; no platform dependencies. Compiles on `wasm32-unknown-unknown`
//!   and runs as native unit tests.
//! - `NodeId` is an index into the `nodes: Vec<Node>` arena. Stable for the
//!   lifetime of the graph (no compaction in this slice).
//! - One root per speaker lane in `roots`. Lanes are addressed by `u8` speaker
//!   index; the lane → speaker binding lives outside this module (in the
//!   session/orchestrator layer per architecture.md).

mod edge;
mod graph;
mod node;

pub use edge::{Edge, EdgeKind};
pub use graph::{SttWord, WordGraph};
pub use node::{FLAG_FILLER, FLAG_TURN_LOCKED, Node, NodeFlags, NodeId, NodeKind, NodeOrigin};
