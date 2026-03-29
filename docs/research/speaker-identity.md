# Speaker Identity & Filtering — Research Notes

> Status: **Research**
> Date: 2026-03-28
> Feeds into: Phase 6 (Diarization & Speaker Identification), Word Graph `ProjectionOpts`

---

## Context

Parley's current multi-speaker mode uses channel separation (mic vs system audio) with named speaker labels. The word graph spec includes `speaker: u8` per node and `ProjectionOpts` with speaker filtering. This research covers how to move from channel-based speaker identity to voiceprint-based speaker identity, and how to filter unwanted speakers at the projection level.

---

## 1. Speaker Enrollment

### Goal
Allow users to enroll speakers at the beginning of a session so that the system can attribute words to named individuals, not just "speaker 0 / speaker 1."

### Enrollment UX

Best approach: a natural prompt that serves double duty (introduction + voiceprint capture).

**Recommended prompt:**
> "Please say your name and one short sentence about yourself."

Example: *"I'm John. I work in robotics and I love hiking."*

This gives:
- Name (for LLM extraction → label assignment)
- 3–5 seconds of natural speech (sufficient for modern embedding models)
- Varied phonemes from conversational speech
- Natural cadence and pitch

**Optional supplementary phrase** (for higher-quality prints):
> "The quick brown fox jumps over the lazy dog."

Not required — modern embedding models (ECAPA-TDNN) produce stable embeddings from ~2 seconds of speech.

### Embedding capture strategy

```
record 3–5 seconds of enrollment speech
  ↓
slice into overlapping chunks (~1.5s each)
  ↓
compute embedding per chunk
  ↓
average embeddings
  ↓
store as speaker voiceprint
```

Averaging over multiple chunks reduces the impact of:
- Momentary noise
- Mic artifacts
- Pitch shifts from nervousness

### Name extraction

Use an LLM to parse the enrollment phrase and extract the speaker name. This is lightweight — a single prompt on a short transcript.

Fallback: manual naming in the UI if LLM extraction fails.

---

## 2. Embedding Model Selection

### Target constraints
- Rust native (no Python runtime)
- WASM capable (edge processing)
- CPU friendly
- Small enough for browser loading

### ECAPA-TDNN (recommended)

- State-of-the-art speaker verification embeddings
- Works with ~1–2 seconds of audio
- Robust to noise
- Small variant is ~20MB (acceptable for session initialization, not per-page-load)
- Can be exported to ONNX format

### Runtime options for Rust/WASM

| Runtime | WASM support | Pure Rust | Notes |
|---|---|---|---|
| `tract` (tract-onnx) | **Yes** | **Yes** | Best option. Pure Rust, full WASM support, loads ONNX models directly |
| `candle` (HuggingFace) | Yes | Yes | Pure Rust ML framework, more work to set up model |
| `ort` (ONNX Runtime) | **Partial** | No (C++ bindings) | `onnxruntime-web` uses JS; native Rust→WASM path is limited |

**Recommendation: `tract`** — pure Rust, proven WASM support, loads ECAPA-TDNN ONNX models without a JS bridge. This aligns with philosophy principle #10 ("no JS bridges, no Python subprocesses, no FFI hairballs").

---

## 3. Speaker Matching

### Cosine similarity

```
sim = dot(a, b) / (|a| * |b|)
```

Basic thresholds (starting point):
- `sim > 0.75` → likely same speaker
- `sim < 0.65` → likely different speaker
- `0.65–0.75` → ambiguous zone

**Important caveat:** These thresholds are oversimplified for adverse conditions. In noisy environments with whispering, similarity scores compress — the gap between "same" and "different" can shrink to ~0.05. Adaptive thresholds calibrated per session during enrollment are needed.

### Multi-embedding matching

Store multiple embeddings per speaker rather than collapsing to one:

```rust
struct SpeakerProfile {
    name: String,
    embeddings: Vec<[f32; 192]>,  // multiple enrollment chunks
}
```

Match by averaging similarity across stored embeddings. This handles variation from:
- Whisper vs normal volume
- Head turns (mic distance changes)
- Background noise fluctuation

---

## 4. Speaker-Aware Filtering

### Core concept

This is a **projection** of the word graph, not a destructive filter. The graph stores all words from all speakers. The UI displays a filtered view based on enrolled speaker identity.

This maps directly to the existing word graph design:
- `ProjectionOpts` already includes speaker filter
- `Node.speaker: u8` stores the diarization-assigned speaker index
- Filtering happens at display time, not at ingest time

### Filter modes

**Whitelist (primary):** Only show words from enrolled speakers. Use case: coffee shop dictation — filter out barista, background conversations.

**Blacklist:** Exclude known unwanted speakers. Use case: TV in background, repeating announcements.

**Show all:** No filtering. Full transcript. Use case: review, legal/compliance.

### Confidence-based combined filtering

```
keep word if:
  speaker_similarity > threshold
  AND word_confidence > min_confidence
```

This combines diarization confidence with transcription confidence for higher-quality output.

### Critical implementation rule

**Never filter audio before ASR.** The correct pipeline is:

```
audio → ASR + diarization → speaker embedding → identity match → filter transcript
```

Filtering audio before transcription breaks diarization — the model needs the full acoustic scene to distinguish speakers.

---

## 5. Identity Scope

### Within-session (initial target)

Speaker IDs are stable within a single recording session. "Speaker 0 = John" holds for the duration of that session. IDs may differ across sessions.

This is sufficient for the current architecture and avoids persistent voiceprint storage.

### Cross-session (future)

Persistent voiceprint database. "John always resolves to John" regardless of session. Requires:
- Secure persistent storage of speaker embeddings
- Similarity search across stored profiles
- Profile management UI (add, remove, merge speakers)
- Privacy considerations (biometric data)

Not needed now. The word graph's `speaker: u8` is session-scoped, which is correct for within-session identity.

---

## 6. "Listen Only to Me" Mode

A compelling special case of whitelist filtering:

```
allowed_speakers = { self }
```

The user enrolls only themselves. All other voices are filtered from the transcript. Effectively turns Parley into a personal dictation tool that works in noisy environments — the user speaks at a coffee shop and only their words appear.

This solves the specific problem described in brainstorming: "when I pause, it picks up other people's voices."

---

## Future Directions

These were discussed in brainstorming but are not actionable now:

- **Phonetically balanced enrollment phrases** — researched but unnecessary with modern embedding models that work from 2s of natural speech
- **Cross-session voiceprint database** — requires persistent biometric storage, privacy policy, profile management UI
- **Semantic gating** (LLM decides whether to keep borderline-confidence speech based on content) — adds latency, introduces hallucination risk, the graph model handles this better via non-destructive storage + user-toggled projections
- **Adaptive threshold learning** — system learns per-user optimal similarity thresholds over time from corrections

---

## Action Items

- [ ] Evaluate `tract` crate for ONNX inference in WASM (load ECAPA-TDNN small)
- [ ] Find/convert ECAPA-TDNN model to ONNX format suitable for tract
- [ ] Prototype cosine similarity matching in Rust (trivial math, needs benchmarking on real embeddings)
- [ ] Design enrollment UI flow (record → extract name → store embedding)
- [ ] Add `speaker_confidence: f32` to `Node` struct (also noted in stt-providers.md)
- [ ] Implement speaker filter in `ProjectionOpts` (graph walk skips non-matching speakers)
