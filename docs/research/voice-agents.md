# Voice Agents & Full-Duplex Conversation — Research Notes

> Status: **Research**
> Date: 2026-03-28
> Feeds into: Phase 8 (Full-Duplex & TTS), `TtsProvider` trait

---

## Context

Parley's todo.md Phase 8 defines full-duplex mode: simultaneous capture + playback, with an AI as a conversation participant. This research covers the technology landscape for making that work, with a focus on Gradium as the leading candidate for the TTS/voice-agent path.

Related Hume work: [hume-octave-2-tts.md](hume-octave-2-tts.md) and [../hume-octave-2-tts-integration-spec.md](../hume-octave-2-tts-integration-spec.md).

---

## 1. Cascaded vs Audio-Native Pipelines

### Cascaded (current industry standard)

```text
User audio → STT → text → LLM → text → TTS → AI audio
```

Latency stack:

- STT: ~200–500ms
- LLM first token: ~200–500ms
- TTS first audio: ~200–400ms
- **Total: ~600–1400ms** (noticeably slower than human turn-taking)

Human conversational turn-taking gap is ~200ms. Cascaded pipelines can't match this.

Additional limitations:

- Non-linguistic information (tone, emotion, hesitation) is lost at the STT→text boundary
- Turn-taking is rigid — can't model overlapping speech, interjections, or backchannels
- Each component is independently optimized, not jointly

### Audio-native (emerging)

```text
User audio tokens → Audio Language Model → AI audio tokens
```

The model processes audio tokens directly, without an intermediate text representation. Text may be generated as an "inner monologue" alongside audio tokens (Moshi's approach) for linguistic grounding, but the primary modality is audio-to-audio.

Advantages:

- Full-duplex by design (model hears while speaking)
- Preserves prosodic/emotional information
- Latency: ~160–200ms theoretical (Moshi achieves 200ms in practice)
- Natural turn-taking, backchannels, and interruptions

---

## 2. Gradium — Evaluation (2026-03-28)

### Company

- **Founded:** 2025, launched December 2025
- **HQ:** Paris
- **Funding:** $70M seed (FirstMark Capital, Eurazeo, DST Global Partners, angels including Yann LeCun)
- **Team size:** ~15–20 (hiring aggressively)

### Founders

| Person | Role | Background | Notable work |
| --- | --- | --- | --- |
| Neil Zeghidour | CEO | Google DeepMind / Meta | **SoundStream** (1432 citations) — invented neural audio codecs with RVQ. Co-authored AudioLM (1070 citations), SoundStorm, LEAF |
| Alexandre Défossez | Chief Science Officer | Meta | **EnCodec** (1451 citations) — Meta's neural audio codec. Lead on **Moshi** (first real-time full-duplex spoken LLM) and **DSM** (Delayed Streams Modeling) |
| Laurent Mazaré | Chief Coding Officer | Google DeepMind / Jane Street | Co-author on Moshi, DSM. Co-created the `candle` Rust ML framework. Maintains `gradium-rs` |
| Olivier Teboul | CTO | Google Brain | Co-authored LEAF with Zeghidour |
| Eugene Kharitonov | Founding Scientist | Meta / Google DeepMind | Co-author on DSM, multiple audio generation papers |

**Assessment:** These founders literally created the two dominant neural audio codecs (SoundStream and EnCodec), then built the first full-duplex speech model (Moshi) at Kyutai. The research credibility is genuine — ~3000+ combined citations on foundational papers.

### Technology: Delayed Streams Modeling (DSM)

Gradium's models are built on the DSM architecture (arXiv:2509.08753), which evolved from the Moshi/RQ-Transformer approach.

**Key idea:** Model text and audio as time-aligned streams with configurable delays using a decoder-only language model.

- TTS = text stream in, delayed audio stream out
- STT = audio stream in, delayed text stream out
- **Same architecture for both directions**

Audio tokenization uses Residual Vector Quantization (RVQ) with up to 32 codebooks per 80ms time-slice. Fewer codebooks = lower latency + lower quality. Configurable at inference time.

### Benchmarked performance (TTS)

TTFA (Time to First Audio) — the metric that matters for conversational feel:

| Provider | p50 | p90 | p99 |
| --- | --- | --- | --- |
| **Gradium** | **255ms** | **263ms** | **274ms** |
| ElevenLabs Turbo v2.5 | 294ms | 311ms | 324ms |
| ElevenLabs Flash v2.5 | 317ms | 333ms | 351ms |
| Mistral Voxtral TTS | 346ms | 400ms | 566ms |
| OpenAI GPT-4o Mini | 400ms | 439ms | 483ms |

With persistent WebSocket (multiplexing, no connection overhead):

| Provider | p50 | p90 |
| --- | --- | --- |
| **Gradium** | **212ms** | **219ms** |
| ElevenLabs Turbo v2.5 | 248ms | 263ms |

Benchmark methodology was rigorous: same input text, same output format, same geographic region, discarded warmup queries.

### API features relevant to Parley

- WebSocket streaming (both TTS and STT)
- TTS: word-level timestamps in audio output (for transcript-audio sync)
- STT: semantic VAD with multi-horizon inactivity probabilities
- Flush support (`flush_id`)
- Multiplexed connections (reuse one WebSocket for multiple sessions)
- Voice cloning (10s enrollment)
- **Rust SDK:** `gradium-rs` (Apache-2.0), maintained by co-founder Laurent Mazaré
- **Regions:** EU and US endpoints
- Deployment: cloud API, dedicated instances, self-hosted, on-premises

### STT limitations (not suitable for Parley's transcription needs)

- No word-level timestamps (segment-level `start_s` only)
- No word-level confidence
- No diarization
- PCM input at 24kHz (Parley uses 16kHz — would need resampling)

See docs/research/stt-providers.md for full STT comparison.

### Where Gradium fits

**TTS for Phase 8 (Full-Duplex & TTS):**

- Sub-250ms TTFA enables near-human turn-taking
- Streaming: LLM tokens → Gradium TTS → audio output with minimal buffering
- Voice cloning could give the AI a consistent, recognizable voice
- Rust SDK simplifies integration

**Not for Phase 2/6 STT** — use Deepgram for that.

---

## 3. Full-Duplex Architecture for Parley

### Cascaded approach (practical near-term)

```text
User mic → Deepgram STT → word graph
                              ↓
                         LLM (text reasoning)
                              ↓
                    Gradium TTS → speaker output
```

This is the standard voice agent pipeline. Latency budget:

- Deepgram STT + VAD: ~300ms
- LLM first token: ~200–400ms
- Gradium TTS first audio: ~250ms
- **Total: ~750–950ms** (acceptable but not conversational)

Optimization: stream LLM tokens directly into Gradium TTS as they arrive, overlapping LLM generation with TTS synthesis.

### Audio-native approach (future, aspirational)

If Gradium ships a full audio-to-audio model (Moshi-derived):

```text
User audio tokens → Gradium ALM → AI audio tokens + text inner monologue
```

This would give ~200ms latency and true full-duplex. Gradium's founding team built Moshi, so this is plausible on their roadmap, but not available via API today.

### AI as conversation participant

When the AI speaks, it becomes another speaker in the word graph:

- AI speech gets its own `speaker: u8` lane
- AI-generated words have `origin: NodeOrigin::AiGenerated` (new variant, if needed)
- AI words appear in the transcript alongside human speakers
- Diarization, timestamping, and projection all work the same way

### Turn-taking / floor management

In multi-party conversations with AI:

- AI should respect semantic VAD — don't interrupt when humans are mid-sentence
- AI should handle backchannels — "yeah", "mm-hmm" from humans shouldn't trigger a full response
- AI should support barge-in — if a human starts speaking while AI is talking, AI stops

Gradium's semantic VAD (multi-horizon inactivity probabilities) provides the signal for these decisions. The `inactivity_prob` at the 2-second horizon is a good turn-taking trigger.

---

## 4. Voice Cloning for AI Persona

Gradium supports instant voice cloning from 10 seconds of audio. This could give Parley's AI assistant a consistent, distinctive voice that users can recognize and customize.

Not a near-term priority, but worth noting that the enrollment audio for speaker identity (docs/research/speaker-identity.md) could double as voice cloning input if Gradium is the TTS provider.

---

## Future Directions

These were raised in brainstorming but are speculative and premature:

- **Multi-AI coordination:** Multiple AI agents in the same conversation, sharing context via a "blackboard" pattern. Real pattern in multi-agent systems but far beyond current scope.
- **AI self-ducking:** AI drops its own volume and rewinds its state when a human interrupts. Natural behavior for a good conversational agent, but requires tight integration between TTS output and diarization input.
- **AI thought stream:** Faint UI showing what the AI is "thinking" before it speaks. Interesting UX concept for future exploration.
- **Proactive AI participation:** AI jumps in with suggestions without being asked. Requires reliable floor management to avoid being annoying. Should be a user-toggled mode, not default behavior.

---

## Action Items

- [ ] Define `TtsProvider` trait (Phase 8 prerequisite)
- [ ] Prototype Gradium TTS integration using `gradium-rs`
- [ ] Test Gradium TTFA from Parley's deployment region
- [ ] Design AI speaker lane in word graph (`speaker: u8` assignment, `NodeOrigin` variant)
- [ ] Implement turn-taking logic using Deepgram/Gradium VAD signals
- [ ] Evaluate LLM token → Gradium TTS streaming pipeline for latency
