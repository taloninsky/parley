# Hume Octave 2 TTS Research

**Status:** Draft
**Type:** Concept
**Audience:** Both
**Date:** 2026-04-26

## Research Requirements

- Evaluate Hume Octave 2 as a Parley `TtsProvider` candidate.
- Feed implementation decisions into [hume-octave-2-tts-integration-spec.md](../hume-octave-2-tts-integration-spec.md).
- Compare fit against Parley's existing TTS direction in [voice-agents.md](voice-agents.md), [conversation-voice-slice-spec.md](../conversation-voice-slice-spec.md), [xai-speech-integration-spec.md](../xai-speech-integration-spec.md), and the provider-neutral expression direction in [conversation-mode-spec.md](../conversation-mode-spec.md).
- Identify concrete integration work, blockers, and validation steps.
- Do not choose a default provider without a listening and latency spike.

## Bottom Line

Hume Octave 2 is worth adding to the TTS shortlist, but I would not make it the default provider yet.

Its best fit is **expressive, emotionally aware persona speech**: coaching, roleplay, narrative assistant responses, and any Parley mode where delivery quality matters more than raw cost. It is less compelling as the default low-cost conversational TTS provider for three reasons that materially affect default-provider eligibility:

- Public pricing is higher than xAI and ElevenLabs.
- Octave 2 is still marked preview.
- Octave 2 is missing pieces of its expressive control surface (natural-language `description` acting instructions and multilingual voice design are documented as coming soon).

The right next move is a small `HumeTts` spike behind the existing `TtsProvider` trait, using a fixed saved voice, `version: "2"`, streaming output, and a measured TTFA/listening comparison against ElevenLabs, xAI, and Gradium.

## What Octave 2 Is

Hume describes Octave as a speech-language model rather than a conventional TTS model. The pitch is that it understands the emotional and semantic content of the text it speaks, then chooses pronunciation, pitch, tempo, emphasis, and delivery style from that context.

Octave 2 is the current preview generation. Public docs and launch material state:

| Capability | Octave 2 state |
| --- | --- |
| Languages | English, Japanese, Korean, Spanish, French, Portuguese, Italian, German, Russian, Hindi, Arabic |
| Model latency | About 100 ms excluding network transit; launch post says responses under 200 ms |
| Streaming first audio | Instant mode usually returns first audio around 200 ms depending on load and input complexity |
| Streaming modes | HTTP streamed JSON, HTTP streamed file, bidirectional WebSocket |
| Non-streaming modes | JSON with base64 audio, file response |
| Output formats | MP3, WAV, PCM |
| Voice library | 100+ Hume voices plus account custom voices |
| Voice cloning | Available by tier; Hume documents high-quality clones from as little as 15 seconds of audio |
| Voice design | Octave 1 voice design creates voices compatible with Octave 2; Octave 2 multilingual voice design is still coming |
| Acting instructions | `speed` and `trailing_silence` work across models; natural-language `description` acting instructions are Octave 1-only as of the docs, with Octave 2 support coming |
| Timestamps | Octave 2 supports word and phoneme timestamps through `include_timestamp_types` |

## API Shape

Authentication is via `X-Hume-Api-Key` for server-side REST. Hume also supports short-lived access tokens for client-side WebSocket use: the proxy exchanges API key + secret key at `POST /oauth2-cc/token`, then the browser can connect with `access_token`; tokens expire after 30 minutes. For Parley, the simpler first implementation should keep Hume calls in the proxy and never expose the API key.

Relevant endpoints:

| Mode | Endpoint | Use in Parley |
| --- | --- | --- |
| Streamed file | `POST https://api.hume.ai/v0/tts/stream/file` | Simplest first adapter: response body is audio bytes. |
| Streamed JSON | `POST https://api.hume.ai/v0/tts/stream/json` | Better second adapter: chunks include base64 audio plus metadata/timestamps. |
| WebSocket | `wss://api.hume.ai/v0/tts/stream/input` | Best fit once Parley streams LLM text into TTS incrementally instead of per-sentence HTTP. |
| Non-streaming JSON | `POST https://api.hume.ai/v0/tts` | Useful for voice design, testing, and offline generation. |
| Voice list | `GET /v0/tts/voices` with `provider=HUME_AI` or `provider=CUSTOM_VOICE` | Voice picker and validation. |

Request fields that matter for Parley:

| Field | Notes |
| --- | --- |
| `version` | Set to `"2"` to opt into Octave 2. Octave 2 requests require a `voice`. |
| `utterances[].text` | Max 5,000 characters per utterance. Parley's sentence/paragraph chunking already keeps below this. |
| `utterances[].voice` | Voice may be referenced by `id`, or by `name` plus `provider` (`HUME_AI` or `CUSTOM_VOICE`). |
| `utterances[].description` | Strong expressive control in Octave 1; Octave 2 support is documented as coming soon. Max 1,000 characters per utterance. Do not depend on it for Octave 2 v1. |
| `utterances[].speed` | Nonlinear scale from `0.5` to `2.0`. Works across models. |
| `utterances[].trailing_silence` | Adds silence after an utterance. Works across models. |
| `context` | Can reference a previous `generation_id` or context utterances to preserve prosody/continuity. Hume warns this can increase generation time. |
| `format` | Selects MP3/WAV/PCM. Verify whether sample rate/bitrate can be pinned to match Parley's current `Mp3_44100_128` silence splicer. |
| `include_timestamp_types` | `word` and `phoneme`, only for Octave 2. Useful for future word-level TTS highlighting. |
| `instant_mode` | Defaults true for streaming. Requires a specified voice and one generation. |

## Fit For Parley

### Strengths

- **Expression is the differentiator.** Parley already anticipates provider-neutral expression annotations in [conversation-mode-spec.md](../conversation-mode-spec.md). Hume is one of the providers that makes that abstraction worthwhile, because it can infer delivery from text and eventually from provider-native acting instructions.
- **Low-latency streaming is plausible.** Instant mode and Octave 2's published latency put Hume in the same conversation as Gradium for interactive TTS, though Parley should measure TTFA from the actual development region before trusting vendor numbers.
- **Word/phoneme timestamps are strategically useful.** [conversation-voice-slice-spec.md](../conversation-voice-slice-spec.md) defers word-level TTS timings. Hume's timestamp support could directly feed future read-along highlighting and transcript/audio alignment.
- **Continuation maps to Parley's chunking problem.** Hume's `context` and `generation_id` model is designed to keep emotional/prosodic continuity across requests, which is exactly the problem Parley hits when synthesizing one sentence or paragraph at a time.
- **Voice design and cloning are product-aligned.** Persona voices can become part of Parley's profile/persona system without tying the core to a single provider.

### Weaknesses

- **Public pricing is premium.** The pricing page lists overage at `$0.15 / 1,000` characters on Creator, `$0.12 / 1,000` on Pro, `$0.10 / 1,000` on Scale, and `$0.05 / 1,000` on Business. Included subscription characters reduce effective cost, but Hume is still a premium provider compared with the current ElevenLabs cost constant and the xAI pricing captured in [xai-speech-integration-spec.md](../xai-speech-integration-spec.md).
- **Octave 2 is preview.** Treat API behavior and quality claims as spike inputs, not settled production assumptions.
- **Octave 2 expressive controls are not fully landed.** Natural-language `description` acting instructions are documented as Octave 1-only for now. Octave 2 can still infer delivery and supports speed/silence, but the provider-native expression translation layer should wait for a verified Octave 2 control surface.
- **No Rust SDK is advertised.** Hume publishes TypeScript, Python, and .NET SDKs plus a CLI and open-source examples; no Rust SDK as of 2026-04-26. A Rust adapter is still straightforward with `reqwest` and a WebSocket client, but we do not get Gradium's `gradium-rs` advantage.
- **Voice cloning and conversion require consent handling.** Parley should not expose clone/conversion workflows until the UI can capture and store explicit user consent/provenance.

## Provider Comparison

| Provider | Best reason to use | Parley status | Main concern |
| --- | --- | --- | --- |
| ElevenLabs | Already implemented; stable streaming MP3 path | Implemented | Current default model intentionally disables expressive tags; continuity is only partially wired. |
| xAI Speech | Cheap unified STT/TTS vendor, strong fit with existing xAI spec | Specified, not implemented ([xai-speech-integration-spec.md](../xai-speech-integration-spec.md)) | New API; quality and protocol details need implementation spike. |
| Gradium | Lowest-latency voice-agent path, Rust SDK, strong full-duplex research pedigree | Researched only ([voice-agents.md](voice-agents.md)) | Less transcript-rich for STT; TTS provider still needs implementation and cost validation. |
| Hume Octave 2 | Most compelling expressive/persona voice candidate; timestamps and continuation are attractive | Researched only (this doc) | Premium pricing, preview status, and Octave 2 control-surface gaps. |

## Integration Direction

This section is intentionally directional. The implementation contract — file paths, request fields, error codes, test cases, trait changes — lives in [hume-octave-2-tts-integration-spec.md](../hume-octave-2-tts-integration-spec.md). The points below describe what kind of integration is feasible and what the API surface implies; they are not a build plan.

- **Phase 1 (feasible against today's trait):** a minimal adapter using `POST /v0/tts/stream/file` with a fixed saved voice, Octave 2, and instant mode. Treats Hume as a generic streaming MP3 source and forwards bytes through the existing pipeline. Does not require trait changes.
- **Phase 2 (better fit):** move to `POST /v0/tts/stream/json` to capture `generation_id` for continuation and optional word/phoneme timestamps. This requires the trait to carry per-chunk metadata and provider continuation state — Parley's current stream is bytes plus a terminal character count.
- **Phase 3 (later):** `wss://api.hume.ai/v0/tts/stream/input` becomes interesting only when Parley streams LLM deltas directly into TTS instead of dispatching per sentence. The current orchestrator, cache, and SSE player are shaped around chunked HTTP synthesis, so WebSocket should not be the first implementation.

Hume exercises corners of the abstraction that ElevenLabs has not. The integration spec captures the trait-evolution decisions; the only research-level claim here is that **Hume cannot be cleanly represented at full fidelity by the current `TtsProvider` trait** — specifically around audio format variants, voice reference shape, returned continuation state, metadata-bearing streams, and a richer expressive-capability signal than a single boolean.

## Validation Plan

Each test names what would be observed and which result would change the recommendation. A research spike that does not pre-commit to falsifiers tends to confirm whatever the author already believed.

1. **Credential/voice setup:** create or select one saved Hume voice compatible with Octave 2; record the voice ID and provider.
2. **Latency spike:** measure TTFA and full-sentence completion for `stream/file`, `stream/json`, and WebSocket from the dev environment. Compare against ElevenLabs, xAI, and Gradium using the same text.
   - *Falsifier:* if median TTFA exceeds **600 ms** from the dev region on `stream/file` with instant mode, drop Hume from the conversational shortlist and keep it only as a non-interactive narration candidate.
3. **Listening test:** run the same assistant responses through all candidate providers. Include neutral explanation, empathetic response, mild humor, urgent instruction, and multi-sentence narrative.
   - *Falsifier:* if Hume does not measurably outperform ElevenLabs and xAI on the empathetic and narrative samples in a blind comparison, the premium-expression positioning fails and there is no reason to add a third TTS provider.
4. **Continuity test:** synthesize a paragraph split into sentence chunks with and without Hume `context`. Listen for prosody jumps and measure latency overhead.
   - *Falsifier:* if `context` adds more than **200 ms** to per-chunk TTFA without an audible continuity benefit, the Phase 2 adapter loses its main reason to exist and Phase 1 becomes the ceiling.
5. **Timestamp test:** request `include_timestamp_types: ["word"]` and verify whether returned timings are stable enough for read-along highlighting.
   - *Falsifier:* if word timestamps drift by more than ~50 ms against perceived audio in informal review, defer the read-along highlighting use case rather than pinning it to Hume.
6. **Cost sanity check:** compute effective per-turn cost against representative Parley assistant responses, not just vendor list prices.
   - *Falsifier:* if effective per-turn cost on the Pro tier exceeds **3x** the projected xAI cost for the same turns, Hume is not a credible default candidate even after quality wins.

## Recommendation

Add Hume Octave 2 as a **premium expressive TTS candidate**, not as the default provider.

For near-term Parley development, Hume is best framed as:

- a listening-quality benchmark for persona voice;
- a future provider-native target for expression annotations;
- a candidate for word-level TTS highlighting because of Octave 2 timestamps;
- a provider to evaluate once the TTS trait grows beyond raw bytes and character counts.

I would prioritize xAI for low-cost default TTS, Gradium for low-latency full-duplex experiments, and Hume for the question: "Can Parley's AI voice sound emotionally intentional instead of merely readable?"

## Sources

Verified 2026-04-26.

- [Hume TTS Overview](https://dev.hume.ai/docs/text-to-speech-tts/overview) — Octave 1 vs Octave 2 capability matrix (languages, latency, voice cloning, voice design, acting instructions, continuation, timestamps), streaming/non-streaming endpoint list, instant mode behavior, and API limits (5,000 char text per utterance, 1,000 char description per utterance, 5 generations per request, MP3/WAV/PCM formats).
- [Hume Pricing](https://www.hume.ai/pricing) — overage rates `$0.15`, `$0.12`, `$0.10`, `$0.05` per 1,000 characters across Creator, Pro, Scale, Business; included character allowances per tier.
- [Hume SDKs (intro)](https://dev.hume.ai/intro#sdks) — published SDKs are TypeScript, Python, .NET, plus a CLI; no Rust SDK as of this date.
- [Hume Continuation guide](https://dev.hume.ai/docs/text-to-speech-tts/continuation) — `context` and `generation_id` model for prosodic continuity across requests.
- [Hume Voice Cloning](https://dev.hume.ai/docs/voice/voice-cloning) — high-quality clones from as little as 15 seconds of audio.
