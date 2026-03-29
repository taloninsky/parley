# STT Provider Comparison — Research Notes

> Status: **Research**
> Date: 2026-03-28
> Feeds into: Phase 2 (Streaming STT), Phase 6 (Diarization), `SttProvider` trait

---

## Context

Parley currently uses AssemblyAI v3 Universal Streaming (`u3-rt-pro`) for real-time STT. The word graph data model (docs/word-graph-spec.md) requires per-word confidence, timestamps, and speaker attribution. This research evaluates whether AssemblyAI meets those requirements or whether a second provider is needed.

---

## Provider Comparison Matrix

| Capability | AssemblyAI v3 | Deepgram Nova | Gradium | WhisperX (self-hosted) |
|---|---|---|---|---|
| Streaming STT | Yes | Yes | Yes | No (batch only) |
| Word-level timestamps | Yes | Yes | No (segment-level `start_s` only) | Yes |
| Word-level confidence | Yes | Yes | No | No (requires custom logprob extraction) |
| Word-level speaker | **No** (turn-level only) | **Yes** | No (no diarization) | Yes (via pyannote alignment) |
| Speaker confidence | No | Yes (`speaker_confidence` per word) | No | No |
| Multi-channel | No | Yes (separate streams per channel) | No | No |
| Overlap handling | Weak (merges speakers) | Better (word-level attribution) | N/A | Good (pyannote) |
| Flush / force endpoint | Yes (`Terminate` / new turn) | Yes (`finalize`) | Yes (`flush` with `flush_id`) | N/A |
| Rust SDK | No (custom WebSocket) | No (custom WebSocket) | Yes (`gradium-rs`, Apache-2.0) | N/A |
| Pricing model | $0.45/hr per session | Per-minute | Credit-based (1s STT = 3 credits) | Free (self-hosted compute) |

### Key finding

**Deepgram is the only provider that returns `speaker` + `confidence` + `speaker_confidence` at the word level** in a single API response. This maps directly to the word graph `Node` struct:

```rust
struct Node {
    confidence: f32,    // ← Deepgram: word.confidence
    speaker: u8,        // ← Deepgram: word.speaker
    start_ms: f64,      // ← Deepgram: word.start
    end_ms: f64,        // ← Deepgram: word.end
    // ...
}
```

A `speaker_confidence: f32` field could be added to `Node` to capture Deepgram's additional signal.

---

## AssemblyAI v3 — Current Provider

### Strengths
- Best raw transcription accuracy (~5.6–6.7% WER in published benchmarks)
- Clean structured API (v3 `Turn` messages with `turn_is_formatted` flag)
- Already integrated and working in Parley
- Good for single-speaker dictation

### Limitations for Parley's roadmap
- Speaker diarization is turn-level only — `Turn` messages contain a transcript string, not individual words with speaker labels
- No way to get word-level speaker attribution even with post-processing
- Overlapping speech handling is weak — tends to merge speakers or miss quiet interjections
- This is a design limitation of their API, not a configuration issue

### Verdict
Keep for single-speaker mode and batch reprocessing (higher accuracy). Not sufficient for word-level diarization requirements.

---

## Deepgram Nova — Recommended Addition

### Strengths
- Word-level speaker attribution in a single API response
- Per-word confidence AND per-word speaker confidence
- Designed for call center + meeting use cases (close to Parley's target)
- Streaming WebSocket API, structurally similar to AssemblyAI
- Binary PCM input supported
- Faster speaker switching (important for interjections like "yeah" mid-sentence)
- Multi-channel aware

### Limitations
- Slightly worse raw WER than AssemblyAI (~8.1–9.2% in published benchmarks, ~2–3% absolute gap)
- WER benchmarks are pure transcription — don't measure diarization accuracy, which is where Deepgram may actually perform better end-to-end

### API shape (relevant fields)

```json
{
  "word": "hello",
  "start": 15.259,
  "end": 15.338,
  "confidence": 0.972,
  "speaker": 0,
  "speaker_confidence": 0.585
}
```

### Integration path
- Implement as second `SttProvider` behind the existing trait boundary
- WebSocket streaming, binary PCM input — similar plumbing to AssemblyAI
- Proxy may need a Deepgram token endpoint (or use API key directly if CORS allows)

---

## Gradium — Not Suitable for STT, Promising for TTS

### STT capabilities (evaluated 2026-03-28)
- Streaming WebSocket STT at `wss://[region].api.gradium.ai/api/speech/asr`
- Returns `text` (string) + `start_s` (float) — **segment-level only**
- No word-level breakdown, no confidence scores, no diarization
- Semantic VAD with multi-horizon inactivity probabilities (useful for turn-taking)
- Supports flush with `flush_id`
- PCM input at 24kHz (Parley currently uses 16kHz — would need resampling)

### Why it's not suitable for STT
Missing all three critical fields for the word graph: per-word timestamps, per-word confidence, per-word speaker. Optimized for voice agent latency, not transcript richness.

### Where it fits
See docs/research/voice-agents.md — Gradium is the leading candidate for Phase 8 (Full-Duplex & TTS).

---

## WhisperX — Batch Fallback Option

### Strengths
- Best available accuracy for batch processing
- Word-level timestamps via forced alignment (wav2vec)
- Word-level speaker via pyannote diarization
- Self-hosted, no per-minute cost
- Highly tunable

### Limitations
- Not real-time — batch only
- No native word-level confidence (must be estimated from token logprobs)
- Requires Python runtime — conflicts with Rust-only philosophy
- Would need to run as a sidecar service, not embedded

### Verdict
Potential batch reprocessing option for "record now, refine later" workflow. Lower priority than Deepgram for real-time.

---

## WER Benchmark Caveats

Published WER numbers (AssemblyAI ~6%, Deepgram ~9%) should be treated as rough guidance, not ground truth:

1. **Benchmarks use clean read speech** — Parley's target is noisy coffee shops, whispering, overlapping speech
2. **Both providers have shipped new models** since the most-cited benchmarks
3. **Diarization changes the equation** — AssemblyAI's accuracy advantage shrinks when speaker attribution is required, because turn-level diarization introduces alignment errors
4. **The right test is on representative audio** — run both providers on actual Parley recordings and compare

---

## Recommended Architecture

```
Real-time (streaming):
  microphone → Deepgram streaming → word-level speaker + confidence → word graph

Batch refinement (optional, future):
  recorded audio → WhisperX or AssemblyAI → higher accuracy transcript → merge

Single-speaker dictation:
  microphone → AssemblyAI v3 (current) → word graph (speaker=0 for all)
```

The `SttProvider` trait makes this a config choice, not a rewrite.

---

## Action Items

- [ ] Implement Deepgram `SttProvider` (WebSocket streaming, binary PCM)
- [ ] Add `speaker_confidence: f32` to word graph `Node` struct
- [ ] A/B test Deepgram vs AssemblyAI on representative noisy/multi-speaker audio
- [ ] Evaluate Deepgram's overlap handling with interjections ("yeah", "mm-hmm")
