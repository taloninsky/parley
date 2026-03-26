# Word Graph Data Model — Specification

> Status: **Draft**
> Author: Gavin + Copilot
> Date: 2026-03-25

---

## Overview

The word graph is the runtime data model that backs the transcript. It replaces the current flat `String` transcript signal and `LiveWord` vector with a graph-native structure that preserves per-word confidence, timing, origin, filler classification, and edit history — all without data loss.

The design is motivated by three goals:

1. **Graph-native from the start.** Parley will integrate into a graph-native system. The runtime data model should be a graph of nodes and edges, not a string that gets parsed and re-serialized.
2. **Non-destructive editing.** Every operation — STT ingest, LLM formatting, user corrections, filler removal — is additive. Original data is always reachable. Nothing is thrown away.
3. **Display is a projection.** The user never sees the graph directly. They see a projection — a walk of the graph with filters applied. Verbatim mode, clean mode, interleaved multi-speaker, single-speaker, confidence-highlighted — all are just different projection configurations over the same graph.

---

### Conceptual overview

```mermaid
graph LR
    subgraph "Speaker 0 (spine)"
        W0["Hello<br/><small>Word · 0.98</small>"]
        P0[",<br/><small>Punct</small>"]
        W1["my<br/><small>Word · 0.99</small>"]
        W2["name<br/><small>Word · 0.91</small>"]
        W3["is<br/><small>Word · 0.97</small>"]
        W4["Gavin<br/><small>Word · 0.95</small>"]
        P1[".<br/><small>Punct</small>"]
    end

    W0 -->|Next| P0 -->|Next| W1 -->|Next| W2 -->|Next| W3 -->|Next| W4 -->|Next| P1
    W2 -.->|Alt| W2a["main<br/><small>Word · 0.62</small>"]

    subgraph "Speaker 1 (spine)"
        W5["Nice<br/><small>Word · 0.97</small>"]
        W6["to<br/><small>Word · 0.99</small>"]
        W7["meet<br/><small>Word · 0.96</small>"]
        W8["you<br/><small>Word · 0.98</small>"]
        P2[".<br/><small>Punct</small>"]
    end

    W5 -->|Next| W6 -->|Next| W7 -->|Next| W8 -->|Next| P2

    W1 -.->|Temporal| W5
    W6 -.->|Temporal| W3

    style W2a fill:#fff3cd,stroke:#856404
    style P0 fill:#e2e3e5,stroke:#6c757d
    style P1 fill:#e2e3e5,stroke:#6c757d
    style P2 fill:#e2e3e5,stroke:#6c757d
```

Each box is a **node**. Solid arrows are **Next** edges (the primary spine). Dotted arrows are **Alt** (alternative transcription) and **Temporal** (cross-speaker timing) edges. Punctuation nodes sit inline on the spine. The Alt branch does not rejoin.

The temporal edges cross in **both** directions (speaker 0→1 and speaker 1→0), creating a DAG with diverge/coalesce structure: "name" and "Nice, to" are **parallel** (spoken simultaneously), then paths converge at "is". After "is", "Gavin" and "meet, you" are parallel again (no temporal edge — they overlap in time). See §7.2 for a detailed walkthrough.

---

## 1. Nodes

### 1.1 NodeKind (enum — mutually exclusive)

A node is exactly one kind. This is an enum because these are mutually exclusive — a node cannot be a Word and Punctuation simultaneously.

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NodeKind {
    Word,         // a spoken or typed word
    Punctuation,  // inline punctuation: . , — ? ! ; : " '
    Silence,      // a timed gap in speech (text is empty, duration in start_ms/end_ms)
    Break,        // explicit line or paragraph break ("\n" or "\n\n")
}
```

**Word** — the primary node type. One per detected word from STT, or per typed word from the user, or per word inserted by the LLM formatting pass.

**Punctuation** — a separate node in the Next chain, not attached to the preceding word. This allows punctuation to have independent confidence (when available), to be independently styled, and to be independently removed or modified.

```mermaid
graph LR
    A["Hello<br/><small>Word</small>"] -->|Next| B[",<br/><small>Punct</small>"] -->|Next| C["world<br/><small>Word</small>"] -->|Next| D[".<br/><small>Punct</small>"]
    style B fill:#e2e3e5,stroke:#6c757d
    style D fill:#e2e3e5,stroke:#6c757d
```

In projection, punctuation is rendered without a preceding space: `Hello, world.`

**Silence** — represents a timed gap between words. `text` is empty. Duration is encoded in `start_ms` / `end_ms`. Used for pause detection, pacing analysis, and paragraph-break heuristics.

**Break** — an explicit structural break in the text. `text` is `"\n"` (line break) or `"\n\n"` (paragraph break). Has an `origin` like any other node — `Stt` for turn boundaries, `LlmFormatted` for breaks inserted by the formatting pass, `UserTyped` for manual Enter presses.

### 1.2 NodeOrigin (enum — mutually exclusive)

Where this node came from. A node has exactly one origin.

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NodeOrigin {
    Stt,           // produced by the STT provider
    LlmFormatted,  // inserted or modified by the LLM formatting pass
    UserTyped,      // typed by the user
}
```

### 1.3 NodeFlags (bitfield — combinable)

Cross-cutting boolean properties that can apply to any node regardless of kind. These are independent of each other and independently combinable. A `u16` provides 16 bits; we define only what we need now and leave room for future use.

```rust
type NodeFlags = u16;

const FLAG_FILLER: NodeFlags = 1 << 0;  // word is a filler (um, uh, er, ah, etc.)
```

When all bits are zero, the node has no special flags — this is the default, zero-cost case.

Future flag candidates (not defined until needed): proper noun, technical term, bold, italic, underline, strikethrough.

### 1.4 Node struct

```rust
type NodeId = u32;

#[derive(Clone, Debug)]
struct Node {
    id: NodeId,
    kind: NodeKind,
    text: String,
    confidence: f32,    // 0.0–1.0, from STT or synthetic
    start_ms: f64,      // timestamp relative to session start
    end_ms: f64,
    speaker: u8,        // lane index (0, 1, ...)
    origin: NodeOrigin,
    flags: NodeFlags,
}
```

**Convenience methods:**

```rust
impl Node {
    fn is_filler(&self) -> bool  { self.flags & FLAG_FILLER != 0 }
    fn set_filler(&mut self)     { self.flags |= FLAG_FILLER; }
    fn clear_filler(&mut self)   { self.flags &= !FLAG_FILLER; }
}
```

---

## 2. Edges

### 2.1 EdgeKind (enum — mutually exclusive)

All edges live in a single collection. An edge has exactly one kind.

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EdgeKind {
    Next,        // primary sequence within a speaker lane (words, punctuation, breaks)
    Alt,         // alternative transcription (branch — lower-confidence option)
    Correction,  // user or LLM edit: old node → replacement node
    Temporal,    // cross-speaker timing link (derived — rebuildable)
}
```

**Next** — the primary spine. Walk `Next` edges from a lane root to traverse the full transcript for that speaker. Words, punctuation, silence, and breaks are all linked by `Next` edges in sequence.

**Alt** — an alternative transcription for a word. Branches off the spine; does not rejoin. Used when the STT provides multiple hypotheses, or when the LLM suggests but doesn't commit a change.

```mermaid
graph LR
    A["name<br/><small>Word · 0.91</small>"] -->|Next| B["is<br/><small>Word</small>"]
    A -.->|Alt| C["main<br/><small>Word · 0.62</small>"]
    style C fill:#fff3cd,stroke:#856404
```

**Correction** — links an old node to its replacement. When the user edits a word or the LLM reformats a passage, the old nodes are bypassed on the spine (their incoming/outgoing `Next` edges are rewired), but they remain in the arena. A `Correction` edge from the first old node to the first replacement node preserves the relationship for undo and provenance.

**Temporal** — a derived edge linking words across speaker lanes by timing. These are artifacts of an analysis pass: computed from word timestamps to establish the interleaving order for multi-speaker projection. They can be deleted and recomputed at any time (e.g., after edits change the timing structure in a region).

### 2.2 Derived vs. intrinsic distinction

This distinction is semantic, not structural. All edges share the same struct and collection. The distinction is expressed by an `EdgeKind` method:

```rust
impl EdgeKind {
    fn is_derived(self) -> bool {
        matches!(self, EdgeKind::Temporal)
    }
}
```

Derived edges can be bulk-cleared and recomputed. Intrinsic edges (`Next`, `Alt`, `Correction`) are part of the core structure and are only modified by explicit graph operations (ingest, edit, undo).

### 2.3 Edge struct

```rust
#[derive(Clone, Debug)]
struct Edge {
    from: NodeId,
    to: NodeId,
    kind: EdgeKind,
}
```

---

## 3. WordGraph

### 3.1 Storage: arena + adjacency

```rust
struct WordGraph {
    nodes: Vec<Node>,       // arena — NodeId is an index into this vec
    edges: Vec<Edge>,       // all edges (intrinsic + derived)
    roots: Vec<NodeId>,     // one per speaker lane; roots[0] = speaker 0's first node
    next_id: NodeId,        // monotonically increasing node ID counter
    free: Vec<NodeId>,      // free list for reusing slots of unreachable nodes (compaction)
}
```

Arena-based: `NodeId` is an index into `nodes`. No `Rc`, no pointer cycles, no borrow checker issues. Cache-friendly iteration. Trivially serializable.

### 3.2 Core operations

```rust
impl WordGraph {
    // ── Construction ──
    fn new() -> Self;

    // ── Ingest ──
    /// Add nodes from an STT turn event. Appends to the speaker's lane.
    fn ingest_turn(&mut self, speaker: u8, words: &[SttWord]);

    // ── Query ──
    /// Walk the primary spine for a speaker, return node IDs in order.
    fn walk_spine(&self, speaker: u8) -> Vec<NodeId>;

    /// Get outgoing edges of a specific kind from a node.
    fn edges_from(&self, node: NodeId, kind: EdgeKind) -> Vec<&Edge>;

    /// Get all nodes below a confidence threshold.
    fn low_confidence_nodes(&self, threshold: f32) -> Vec<NodeId>;

    // ── Projection ──
    /// Walk the graph and produce a flat text string, applying filters.
    fn project(&self, opts: &ProjectionOpts) -> String;

    /// Interleaved multi-speaker projection using Temporal edges.
    fn project_interleaved(&self, opts: &ProjectionOpts) -> String;

    // ── Editing ──
    /// Replace a span of nodes with new text. Old nodes are bypassed,
    /// not deleted. Correction edge links old → new. Returns IDs of new nodes.
    fn replace_span(&mut self, start: NodeId, end: NodeId, new_text: &str, origin: NodeOrigin) -> Vec<NodeId>;

    /// Delete a span (bypass on spine, no replacement). Correction edge
    /// from predecessor to successor marks the deletion point.
    fn delete_span(&mut self, start: NodeId, end: NodeId);

    /// Insert new text after a node. Returns IDs of new nodes.
    fn insert_after(&mut self, after: NodeId, text: &str, origin: NodeOrigin) -> Vec<NodeId>;

    // ── Analysis ──
    /// Clear all Temporal edges and recompute from word timestamps.
    fn analyze_temporal(&mut self);

    /// Recompute Temporal edges only within a time window.
    fn reanalyze_range(&mut self, start_ms: f64, end_ms: f64);

    // ── Filler detection ──
    /// Scan all Word nodes against a filler word list and set FLAG_FILLER.
    fn detect_fillers(&mut self, filler_words: &[&str]);
}
```

### 3.3 SttWord — input from STT provider

```rust
struct SttWord {
    text: String,
    start_ms: f64,
    end_ms: f64,
    confidence: f32,
}
```

This is what the AssemblyAI v3 `words` array provides per word in a Turn event. The `ingest_turn` method converts these into `Node` entries with `origin: Stt`, detects fillers, creates punctuation nodes for trailing punctuation, and wires `Next` edges.

### 3.4 ProjectionOpts — filters for display

```rust
struct ProjectionOpts {
    include_fillers: bool,         // false = skip nodes with FLAG_FILLER
    include_silence: bool,         // false = skip Silence nodes
    speaker_filter: Option<u8>,    // Some(n) = only speaker n; None = all speakers
    include_speaker_labels: bool,
    include_timestamps: bool,
    confidence_threshold: f32,     // for downstream highlighting (not filtering)
}
```

The projection walk:
1. Start at `roots[speaker]` (or each root if `speaker_filter` is None).
2. Follow `Next` edges along the spine.
3. Skip nodes based on filters (fillers, silence).
4. Emit text, inserting spaces between words, no space before punctuation.
5. For interleaved mode, follow `Temporal` edges to switch speaker lanes at the right moments.

---

## 4. Non-Destructive Editing

### 4.1 Additive model

When the user selects a span of words and types a replacement:

1. The selected nodes' `Next` chain is **bypassed** — the predecessor's `Next` edge is rewired to point past the selection, and the successor is reached by the newly inserted nodes.
2. New nodes are created with `origin: UserTyped`.
3. A `Correction` edge links the first bypassed node to the first new node.
4. The bypassed nodes **remain in the arena** — they are no longer reachable from the active spine, but they are still present.

**Before edit:**

```mermaid
graph LR
    A["The<br/><small>Word</small>"] -->|Next| B["quik<br/><small>Word · 0.72</small>"] -->|Next| C["brown<br/><small>Word · 0.94</small>"] -->|Next| D["fox<br/><small>Word</small>"]
    style B fill:#f8d7da,stroke:#842029
```

User selects "quik brown" and types "quick red":

**After edit:**

```mermaid
graph LR
    A["The<br/><small>Word</small>"] -->|Next| E["quick<br/><small>UserTyped · 1.0</small>"] -->|Next| F["red<br/><small>UserTyped · 1.0</small>"] -->|Next| D["fox<br/><small>Word</small>"]

    B["quik<br/><small>bypassed</small>"] -->|Next| C["brown<br/><small>bypassed</small>"]
    B -.->|Correction| E

    style B fill:#e2e3e5,stroke:#6c757d,stroke-dasharray: 5 5
    style C fill:#e2e3e5,stroke:#6c757d,stroke-dasharray: 5 5
    style E fill:#d1e7dd,stroke:#0f5132
    style F fill:#d1e7dd,stroke:#0f5132
```

The bypassed nodes (dashed borders) remain in the arena. The `Correction` edge allows undo and provenance tracking.

### 4.2 Undo

Undo reverses the spine rewiring: restore the original `Next` edges, remove the new nodes from the spine. The `Correction` edge allows finding the old nodes. For multi-word edits, all nodes involved in a single user action share a logical edit group (an incrementing edit counter stored on the `Correction` edge or as a side-table).

### 4.3 LLM formatting as an edit

When the LLM formatting pass rewrites a passage, it follows the same additive model:

1. Old nodes are bypassed (not deleted).
2. New nodes are created with `origin: LlmFormatted`.
3. `Correction` edges link old to new.

This means "revert to STT original" for any passage is: walk `Correction` edges backward, restore the bypassed nodes to the spine.

### 4.4 Filler handling

Fillers are **not edited out** of the graph. They remain as normal nodes with `FLAG_FILLER` set. "Remove fillers" is a projection-time filter: `if node.is_filler() && !opts.include_fillers { skip }`. Toggling verbatim mode re-projects the same graph with `include_fillers: true`.

**Graph (always the same):**

```mermaid
graph LR
    A["So"] -->|Next| B["um<br/><small>🏳 FILLER</small>"] -->|Next| C["the"] -->|Next| D["idea"] -->|Next| E["is"]
    style B fill:#fff3cd,stroke:#856404,stroke-dasharray: 5 5
```

**Projection (clean mode, `include_fillers: false`):** `So the idea is`

**Projection (verbatim mode, `include_fillers: true`):** `So um the idea is`

Same graph, different projection.

---

## 5. Filler Detection

### 5.1 Filler word list

A static default list of filler words that are almost never intentional content:

```rust
const DEFAULT_FILLERS: &[&str] = &[
    "um", "uh", "hmm", "mm", "er", "ah", "eh", "uh-huh", "mm-hmm",
];
```

### 5.2 Detection at ingest

When `ingest_turn` processes STT words, each word's lowercased `text` is checked against the filler list. If it matches, `FLAG_FILLER` is set on the node.

### 5.3 User-configurable fillers

The filler list is a setting (stored as a cookie, comma-separated). Users can add words. The detection runs at ingest time, so changing the list only affects new words. A `redetect_fillers` operation can re-scan existing nodes if the user changes the list mid-session.

---

## 6. Confidence

### 6.1 Per-node confidence

Every node has a `confidence: f32` field (0.0–1.0).

- **STT-origin nodes**: confidence comes directly from the STT provider's word-level data.
- **LLM-origin nodes**: synthetic confidence (e.g., 1.0 for words the LLM chose, or a value derived from the LLM's stated uncertainty if available).
- **User-typed nodes**: confidence = 1.0 (the user knows what they meant).
- **Punctuation nodes**: confidence from STT if available, otherwise synthetic (e.g., 0.95 for LLM-inserted punctuation).

### 6.2 Confidence threshold

A user setting (default: 0.85). Used at projection/render time to determine which nodes get visual highlighting. The graph doesn't filter low-confidence nodes out — it flags them for the UI layer.

### 6.3 LowConfidence as a query, not an entity

The architecture doc described `LowConfidence` as a separate annotation type. In the word graph, "low confidence" is simply `node.confidence < threshold` — a predicate, not stored data. No separate entity needed.

---

## 7. Multi-Speaker: Forest → DAG

### 7.1 Per-speaker trees

Each speaker lane is a tree rooted at `roots[speaker_index]`. The tree is mostly a spine (linked list via `Next` edges) with occasional `Alt` branches.

### 7.2 Temporal edges make it a DAG

When temporal analysis runs, it adds `Temporal` edges between words on different speaker lanes that overlap in time. This converts the forest into a DAG (directed acyclic graph) — no cycles (time flows forward), but nodes have cross-lane connections.

Consider this scenario where speaker 1 starts responding while speaker 0 is still talking, and both finish at roughly the same time:

```
Timeline (ms):  0    500  1000 1500 2000 2500 3000 3500 4000
Speaker 0:      Hello my   name ···· is   Gavin
Speaker 1:                Nice to   ···· meet you
```

The temporal edges encode the ordering constraints the timestamps reveal:

```mermaid
graph LR
    subgraph "Speaker 0"
        A0["Hello<br/><small>0–500</small>"] -->|Next| A1["my<br/><small>500–1000</small>"] -->|Next| A2["name<br/><small>1000–1600</small>"] -->|Next| A3["is<br/><small>2200–2600</small>"] -->|Next| A4["Gavin<br/><small>2600–3200</small>"]
    end

    subgraph "Speaker 1"
        B0["Nice<br/><small>1100–1500</small>"] -->|Next| B1["to<br/><small>1500–1900</small>"] -->|Next| B2["meet<br/><small>2500–3000</small>"] -->|Next| B3["you<br/><small>3000–3400</small>"]
    end

    A1 -.->|Temporal| B0
    B1 -.->|Temporal| A3

    style A0 fill:#cfe2ff,stroke:#084298
    style A1 fill:#cfe2ff,stroke:#084298
    style A2 fill:#cfe2ff,stroke:#084298
    style A3 fill:#cfe2ff,stroke:#084298
    style A4 fill:#cfe2ff,stroke:#084298
    style B0 fill:#fff3cd,stroke:#856404
    style B1 fill:#fff3cd,stroke:#856404
    style B2 fill:#fff3cd,stroke:#856404
    style B3 fill:#fff3cd,stroke:#856404
```

Blue = speaker 0, amber = speaker 1.

This DAG has a **diverge → coalesce → diverge** structure:

#### Phase 1 — Divergence at "my"

"my" has two outgoing paths: `my → name` (spine) and `my → Nice` (temporal). After this point, speaker 0 ("name") and speaker 1 ("Nice, to") are **running in parallel** — they were spoken simultaneously and the DAG has no ordering between them.

#### Phase 2 — Coalescence at "is"

"is" has two incoming paths: `name → is` (spine) and `to → is` (temporal). Both paths converge. This means "is" doesn't happen until both "name" and "to" are done. This is the structural signal that the overlap region has ended and the speakers are momentarily synchronized.

#### Phase 3 — Divergence again

After the coalescence, "Gavin" continues on speaker 0's spine and "meet → you" continues on speaker 1's spine. There are no temporal edges between these tails — because they genuinely overlap in time (2500–3400ms). The DAG correctly leaves them **unconstrained**, meaning they are parallel.

> **Key principle: absence of temporal edges = parallelism.** If there is no directed path between two nodes in the DAG, they are concurrent. This is not a gap in the data — it is an accurate statement that the speakers were talking at the same time.

#### 7.2.1 The diverge/coalesce pattern

This three-phase pattern recurs throughout any multi-speaker conversation:

1. **Sequential** — one speaker is talking, the other is silent. A single path through the DAG.
2. **Divergence** — the second speaker starts while the first is still talking. A temporal edge marks the transition point; after it, both spines advance in parallel.
3. **Coalescence** — the overlap ends. A temporal edge from the speaker who finishes first to the next word of the other speaker merges the paths. The DAG returns to a single path.

Repeated diverge/coalesce cycles make the DAG look like a braid. Each overlap region is a parallel section between a pair of temporal edges pointing in opposite directions.

#### 7.2.2 Temporal edge density

`analyze_temporal` places edges **at transition points** — the moments where ordering changes between speakers. It does NOT place an edge between every pair of overlapping words (that would be O(n²) and mostly redundant). The rules:

- **Divergence edge** (0→1): placed when speaker 1's first word in an overlap region starts after a word on speaker 0's spine. One edge, at the onset.
- **Coalescence edge** (1→0 or 0→1): placed when one speaker's last word in an overlap region ends before the other speaker's next word. One edge, at the offset.
- **Within a parallel region**: no temporal edges. The two spines are intentionally unconstrained — they are concurrent.

This sparse placement is sufficient to reconstruct the full ordering via topological sort. Dense timestamps are still available on every node for fine-grained alignment in the UI.

### 7.3 Temporal edges are derived

Temporal edges are analysis artifacts. They are computed from word timestamps and can be:
- **Cleared entirely**: `edges.retain(|e| e.kind != EdgeKind::Temporal)`
- **Recomputed for a range**: clear temporals in a time window, then re-derive from timestamps in that window
- **Recomputed globally**: clear all, re-derive from all word timestamps

This means editing a word's timing (or inserting/deleting words) doesn't corrupt the temporal structure — you just reanalyze the affected region. The interleaved projection is always rebuildable.

### 7.4 Overlap rendering strategies

When the projection walk hits a parallel region (a diverge/coalesce pair), it must decide how to render the overlap. This is context-dependent — the UI can do things plain text cannot.

```rust
enum OverlapRendering {
    /// Side-by-side columns aligned by time (for UI / HTML).
    ParallelLanes,
    /// Earliest speaker gets a complete block; other speaker follows
    /// with an [overlap] marker (for plain-text export / clipboard).
    SequentialWithMarker,
    /// Word-by-word interleave by start_ms (rarely readable).
    WordInterleave,
}
```

`project_interleaved` returns structured data, not a flat string:

```rust
enum ProjectedBlock {
    /// Normal single-speaker segment (no overlap).
    Sequential { speaker: u8, text: String },
    /// Two (or more) speakers talking simultaneously.
    Parallel { blocks: Vec<(u8, String)> },
}
```

**UI rendering (ParallelLanes):** A `Sequential` block is a normal paragraph with a speaker label. A `Parallel` block is rendered as side-by-side columns — the user can visually see that both speakers were talking at the same time. Timestamps on each word allow fine-grained vertical alignment within the columns if desired.

For the example in §7.2, the UI would render something like:

```
Speaker 0: Hello, my—
     ┌─────────────────────────┐
     │ Speaker 0: name         │  Speaker 1: Nice to
     └─────────────────────────┘
Speaker 0: is—
     ┌─────────────────────────┐
     │ Speaker 0: Gavin        │  Speaker 1: meet you
     └─────────────────────────┘
```

**Plain-text serialization (SequentialWithMarker):** Parallel blocks are linearized. The speaker whose first word has the earliest `start_ms` goes first as a complete block. The other speaker's block follows, prefixed with `[speaking simultaneously]`:

```
Speaker 0: Hello, my name
Speaker 1: [speaking simultaneously] Nice to
Speaker 0: is Gavin
Speaker 1: [speaking simultaneously] meet you
```

**Word interleave (WordInterleave):** Strict `start_ms` ordering across all speakers, word by word. Produces output like `Hello my name Nice to is meet Gavin you`. Almost never readable; exists as an option for completeness or for downstream tools that want raw temporal order.

Without temporal edges, the fallback is to walk each speaker's spine independently and merge by timestamp — equivalent to `SequentialWithMarker` but using raw timestamps instead of graph structure.

---

## 8. Relation to Architecture.md Entities

How word graph concepts map to the architecture's entity model:

| Architecture entity | Word graph realization | Notes |
|---|---|---|
| **Word** annotation | `Node { kind: Word }` | Identical. The node IS the word annotation. |
| **Phrase** annotation | Contiguous run of nodes between Break nodes | Emergent — a span query, not a stored entity. |
| **Silence** annotation | `Node { kind: Silence }` | Identical. |
| **LowConfidence** annotation | `node.confidence < threshold` | Subsumed by per-node confidence. Not a separate entity. |
| **PostProcessed** annotation | Nodes with `origin: LlmFormatted` + Correction edges to originals | Structural, not a separate annotation layer. |
| **UserCorrection** annotation | `Correction` edge + bypassed nodes with `origin: Stt` or `LlmFormatted` | Structural. Old and new are both in the graph. |
| **Lane** | `speaker` field on nodes + `roots` vec | Implicit in graph structure, not a wrapping entity. |
| **`responds_to`** edge | Future `EdgeKind::RespondsTo` variant | Would be a derived edge kind from an LLM semantic analysis pass. |
| **SpeakerIdentity** | Side table: `speaker_id → SpeakerInfo { name, confidence, method }` | Not in the word graph — session-level metadata. |
| **SpatialPosition** | Not modeled yet | Future: either per-node property or derived annotation. |
| **ConversationGroup** | Not modeled yet | Future: derived from spatial + semantic analysis. |

---

## 9. Settings

New settings in the settings drawer, under a **"Transcript Quality"** section (visible when an STT key is set):

| Setting | Type | Default | Cookie |
|---|---|---|---|
| Strip filler words | checkbox | on | `parley_strip_fillers` |
| Custom filler words | text (comma-separated) | *(empty)* | `parley_custom_fillers` |
| Highlight low-confidence words | checkbox | on | `parley_highlight_confidence` |
| Confidence threshold | number (0.0–1.0) | 0.85 | `parley_confidence_threshold` |
| Verbatim mode | checkbox | off | `parley_verbatim` |

**Verbatim mode** is a master override: when on, it forces `include_fillers: true` in the projection and disables auto-formatting. The other settings are grayed out. The confidence highlighting can remain active in verbatim mode (it's informational, not a cleanup action).

---

## 10. Implementation Sequence

1. **Define `Node`, `Edge`, `WordGraph` structs** — pure data, no UI dependency. New module: `src/graph/mod.rs`.
2. **Parse word-level data from AssemblyAI Turn events** — extend `on_transcript` callback to receive `Vec<SttWord>` alongside the flat transcript string.
3. **`ingest_turn`** — populate the graph from STT words. Detect fillers. Create punctuation nodes.
4. **`project()`** — flat text serialization for display and LLM input. Replaces the `Signal<String>` transcript.
5. **Wire into UI** — graph becomes the backing store. Transcript display reads from `project()`. Current turn boxes continue to show live partials.
6. **Filler detection + settings** — strip-filler toggle, custom word list.
7. **Confidence highlighting** — render low-confidence words with visual indicator (requires moving from textarea to contenteditable or a rendered div).
8. **Temporal analysis + interleaved projection** — replace the current `LiveWord` / `render_live_zone` system.
9. **Non-destructive editing** — user corrections create Correction edges, bypassed nodes preserved.
10. **LLM formatting as graph edit** — formatting pass writes back to graph via `replace_span`, not by overwriting a string.
