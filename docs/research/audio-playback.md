# Audio Playback Processing — Research Notes

> Status: **Research**
> Date: 2026-03-28
> Feeds into: Philosophy #3 (Audio Deserves Respect), Philosophy #12 (Transcript and Audio Are Linked)

---

## Context

Parley's philosophy treats audio as a first-class artifact linked to the transcript. The word graph provides per-word timestamps, speaker identity, and confidence — all of which can drive intelligent playback processing. This research covers three playback features: identity-based gain control, pace normalization, and intelligent silence truncation.

All processing described here is **non-destructive** and computed in a **background thread**. The original audio is never modified. Playback applies computed metadata (gain maps, speed maps) in real time.

---

## 1. Identity-Based Gain Control (Audio Ducking)

### Concept

Use speaker identity from the word graph to generate a volume automation curve. Enrolled speakers play at full volume; unenrolled speakers are ducked to a low ambient level.

This is a **projection of the word graph onto audio gain** — the same graph walk that produces filtered text can produce a gain envelope.

### Gain map structure

```
struct GainSegment {
    start_ms: f64,
    end_ms: f64,
    gain: f32,        // 0.0–1.0
    reason: GainReason,
}

enum GainReason {
    EnrolledSpeaker,
    UnknownSpeaker,
    Overlap,
    Silence,
}
```

The gain map is a flat list of segments, trivially derived from the word graph by walking nodes and checking `speaker` against the enrolled speaker set.

### Rules

| Condition | Gain | Rationale |
|---|---|---|
| Enrolled speaker | 1.0 (100%) | Full volume for wanted speech |
| Unenrolled speaker, temporally separated | 0.15–0.20 (15–20%) | Room tone preserved, voice becomes distant murmur |
| Overlapping speech (enrolled + unenrolled simultaneous) | 1.0 (100%) | Can't duck one without ducking the other on a single channel |
| Silence / ambient | 0.15–0.20 | Consistent room tone floor |

**Never duck to 0.0.** Complete silence creates a "vacuum effect" — the room tone disappears and snaps back, which is jarring. A floor of ~15% preserves ambient continuity.

### Crossfade

Gain transitions require smooth crossfades to avoid audible clicks:
- **Crossfade duration:** 50–100ms
- **Curve:** linear or cosine (cosine sounds smoother)
- **Implementation:** interpolate between adjacent gain segments

### Safety buffer

Add ~500ms padding around enrolled speaker segments before starting a duck. Diarization timestamps can clip trailing audio (breaths, mouth sounds, lingering vowels). The padding prevents unnatural cutoffs.

### Temporal separation rule

Only duck if there's sufficient time gap between enrolled and unenrolled speech. If the gap is < ~200ms, the constant gain pumping (up-down-up-down) is more annoying than leaving the background in.

### Overlap handling

When enrolled and unenrolled speakers overlap in time, keep gain at 1.0. Source separation (unmixing overlapping voices) exists in research but introduces artifacts and is computationally heavy for WASM. The pragmatic approach: if they overlap, you can't duck without damaging your own audio, so don't try.

### Playback modes

Users should be able to toggle gain filtering:

- **Focus mode:** Gain map applied — enrolled speakers prominent, background ducked
- **Full mode:** Gain map ignored — everything at 1.0, hear the complete recording
- **Slider (optional):** "Background Voice Level" — continuous control from 0% to 100% for unenrolled speakers

### Architecture

```
word graph
  ↓
gain map generator (background thread)
  ↓
gain map (Vec<GainSegment>)
  ↓
playback engine applies gain in real time
```

The gain map is precomputed and stored alongside the audio. Toggling modes just swaps whether the map is applied.

---

## 2. Pace Normalization (Time-Scale Modification)

### Concept

Normalize speaking pace across all speakers to a target WPM, then allow a global speed multiplier. Fast talkers slow down, slow talkers speed up. The user hears consistent, controllable pace.

### WPM calculation

The word graph provides start/end timestamps per word. Computing instantaneous WPM per speaker segment is straightforward:

```
segment_wpm = word_count / (segment_duration_seconds / 60)
```

Compute per speaker, per segment (e.g., per turn or per sentence).

### Speed factor

For each segment:

```
S = (WPM_target / WPM_observed) × M_global
```

Where:
- `WPM_target` = user's preferred base pace (default: ~150 WPM, configurable)
- `WPM_observed` = actual pace of that speaker in that segment
- `M_global` = user's master speed slider (1.0x, 1.5x, etc.)

### Speed map structure

```
struct SpeedSegment {
    start_ms: f64,
    end_ms: f64,
    speed_factor: f32,
}
```

Analogous to the gain map — precomputed from the word graph, applied at playback.

### Time-stretch algorithm

**WSOLA (Waveform Similarity Overlap-Add)** — the standard algorithm used by every podcast app. Sounds natural up to ~1.8x. Trivially implementable in Rust, no neural model needed.

For extreme speeds (>2x), a phase vocoder with identity phase locking handles the upper range. Still well-understood signal processing, no ML required.

Neural time-stretch models exist but are overkill for this use case — WSOLA is the right tool.

### UI

- **Checkbox:** "Normalize pace" (on/off)
- **Slider:** Global speed multiplier (0.5x – 3.0x)
- **Optional presets:**
  - Review (1.0x normalized)
  - Skim (1.5x normalized)
  - Archive (raw, no changes)

### Minimum speed change threshold

Don't apply time-stretch for tiny differences. If a speaker is at 145 WPM and the target is 150 WPM, the 1.03x stretch adds processing for no perceptible benefit. Apply a dead zone (e.g., only stretch if factor > 1.1 or < 0.9).

---

## 3. Intelligent Silence Truncation (Supercut Mode)

### Concept

Collapse non-speech gaps to create a dense, information-packed playback. Three levels of aggressiveness:

### Truncation levels

| Level | Behavior | Use case |
|---|---|---|
| Off | Original timing preserved | Archival, legal |
| Standard | Silence shortened to ~300ms "breath" gaps | Normal review |
| Supercut | Silence minimized, unenrolled speech time-collapsed, maximum density | Rapid skim |

### What counts as trimmable

The word graph's `Silence` nodes and `Break` nodes carry timing. Additionally:

- **Structural silence:** gaps between words within a speaker's turn → shorten to ~300ms
- **Turn gaps:** silence between different enrolled speakers → shorten to ~500ms (preserve conversational rhythm)
- **Unenrolled segments:** if gain map marks a segment as unenrolled and it's not overlapping enrolled speech → time-collapse entirely in supercut mode
- **Pensive pauses:** intentional pauses after questions or before key statements → optionally preserve (simple heuristic: silence following a question mark node, keep at least 50% of original duration)

### Minimum duck duration

If the gap between two enrolled-speaker segments is very short (< ~2 seconds), don't time-collapse the unenrolled segment in between. The constant temporal disruption is worse than leaving a 1.5-second gap of background noise.

### Implementation

Silence truncation is another map:

```
struct TimeMap {
    segments: Vec<TimeSegment>,
}

struct TimeSegment {
    source_start_ms: f64,
    source_end_ms: f64,
    target_start_ms: f64,
    target_end_ms: f64,
}
```

This remaps source audio time to playback time. The playback engine seeks and crossfades according to the map.

---

## Composing the Three Features

Gain map, speed map, and time map can be composed. The processing order:

1. **Time map** — which segments to include, how to remap time
2. **Speed map** — time-stretch the included segments
3. **Gain map** — apply volume envelope

All three are precomputed from the word graph in a background thread. Playback applies them in sequence. Toggling any feature just swaps which maps are active.

---

## Future Directions

These were discussed in brainstorming but are speculative:

- **Source separation / target speaker extraction (TSE):** Neural models (e.g., VoiceFilter-Lite) that isolate a single speaker's voice from a mix. Exists in research, produces artifacts, computationally heavy for WASM. Not practical now. The temporal-separation rule handles the 80% case without artifacts.
- **Frequency-specific ducking:** Instead of ducking entire volume, duck only the frequency bands where the unenrolled speaker's voice is strongest. More transparent but requires spectral analysis per segment. Possible future enhancement.
- **Spatial inference from single mic:** Inferring speaker distance/position from room reverb. Research concept, not productizable.
- **Neural TSM:** Using a neural model for time-stretching instead of WSOLA. Only matters at >2x speed. WSOLA is fine for now.
- **Information density heatmap:** Visual overlay on waveform showing where high-information segments are (high WPM + keyword density). Simple to compute from word graph; useful but lower priority than the core playback features.
- **Semantic importance scoring:** LLM-based scoring of transcript segments for importance. Latency and cost concerns. Possible as a batch post-processing step but premature for current architecture.
- **Emotional/prosodic analysis:** Detecting stress, excitement, hesitation from audio features. Research-grade, not productizable into Parley's current scope.

---

## Action Items

- [ ] Implement `GainSegment` generator as a word graph projection
- [ ] Implement crossfade logic (50–100ms cosine)
- [ ] Implement WSOLA time-stretch in Rust (well-documented algorithm)
- [ ] Design playback UI controls (focus/full toggle, speed slider, normalize checkbox)
- [ ] Integrate with transcript-audio linking (Philosophy #12 — click word, hear it)
