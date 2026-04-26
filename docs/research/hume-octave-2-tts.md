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

Its best fit is **expressive, emotionally aware persona speech**: coaching, roleplay, narrative assistant responses, and any Parley mode where delivery quality matters more than raw cost. It is less compelling as the default low-cost conversational TTS provider because the public pricing is materially higher than xAI and ElevenLabs, Octave 2 is still marked preview, and some of the most interesting control surfaces are not fully available on Octave 2 yet.

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
| Voice cloning | Available by tier; guided recording flow usually under 30 seconds; docs and launch copy cite 15 seconds of audio for high-quality clones |
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
| `utterances[].description` | Strong expressive control in Octave 1; Octave 2 support is documented as coming soon. Do not depend on it for Octave 2 v1. |
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
- **No Rust SDK is advertised.** Hume has TypeScript, Python, .NET, Swift, CLI, and examples. A Rust adapter is still straightforward with `reqwest` and a WebSocket client, but we do not get Gradium's `gradium-rs` advantage.
- **Voice cloning and conversion require consent handling.** Parley should not expose clone/conversion workflows until the UI can capture and store explicit user consent/provenance.

## Provider Comparison

| Provider | Best reason to use | Parley status | Main concern |
| --- | --- | --- | --- |
| ElevenLabs | Already implemented; stable streaming MP3 path | Current `TtsProvider` | Current default model intentionally disables expressive tags; continuity is only partially wired. |
| xAI Speech | Cheap unified STT/TTS vendor, strong fit with existing xAI spec | Specified in [xai-speech-integration-spec.md](../xai-speech-integration-spec.md) | New API; quality and protocol details need implementation spike. |
| Gradium | Lowest-latency voice-agent path, Rust SDK, strong full-duplex research pedigree | Researched in [voice-agents.md](voice-agents.md) | Less transcript-rich for STT; TTS provider still needs implementation and cost validation. |
| Hume Octave 2 | Most compelling expressive/persona voice candidate; timestamps and continuation are attractive | New candidate | Premium pricing, preview status, and Octave 2 control-surface gaps. |

## Integration Sketch

### Minimal Adapter

1. Add `ProviderId::Hume` in `proxy/src/providers.rs` with env var `PARLEY_HUME_API_KEY`.
2. Add `proxy/src/tts/hume.rs` implementing `TtsProvider`.
3. Use `POST /v0/tts/stream/file` first:
   - `version: "2"`
   - `instant_mode: true`
   - `num_generations: 1`
   - one `utterances` item per Parley chunk
   - fixed saved voice by ID
   - MP3 format
4. Return `TtsChunk::Audio(bytes)` for response chunks and `TtsChunk::Done { characters }` at EOF.
5. Set `supports_expressive_tags()` to `false` initially. Hume's inline `[pause]` support is not the same as ElevenLabs v3 tag syntax, and Octave 2 natural-language acting instructions need verification.
6. Add mock HTTP tests matching the ElevenLabs adapter pattern.

### Likely Trait Changes

Hume exposes useful features that the current trait cannot represent cleanly:

| Needed change | Why |
| --- | --- |
| Add audio format variants or make `AudioFormat` structured | Hume examples return MP3 at 48 kHz by default; Parley currently only models `Mp3_44100_128`. |
| Add provider-specific voice reference shape | Hume voices can be `{ id }` or `{ name, provider }`; `voice_id: String` is enough for a spike but too narrow for UI. |
| Let `Done` carry provider continuation state | Hume continuation wants a prior `generation_id`; the current `ProviderContinuationState` can be passed in but not returned. |
| Support metadata-bearing audio streams | Streamed JSON carries audio plus timestamps; Parley's current stream only carries raw audio and a terminal character count. |
| Replace boolean `supports_expressive_tags()` with provider expression capabilities | Hume style prompts, xAI/ElevenLabs tags, SSML, and speed/silence controls are different native targets. |

### Better Adapter After Minimal Spike

Use `POST /v0/tts/stream/json` instead of streamed file. Decode base64 audio chunks into `TtsChunk::Audio`, capture `generation_id` for continuation, and optionally capture word/phoneme timestamps for future highlighting. This is the adapter shape that actually takes advantage of Hume rather than treating it as generic MP3 TTS.

### WebSocket Adapter Later

`wss://api.hume.ai/v0/tts/stream/input` becomes interesting when Parley moves from sentence-level HTTP dispatch to streaming LLM deltas directly into TTS. That should not be the first implementation because Parley's current orchestrator, cache, and SSE player are already shaped around chunked HTTP synthesis.

## Validation Plan

1. **Credential/voice setup:** create or select one saved Hume voice compatible with Octave 2; record the voice ID and provider.
2. **Latency spike:** measure TTFA and full-sentence completion for `stream/file`, `stream/json`, and WebSocket from the dev environment. Compare against ElevenLabs, xAI, and Gradium using the same text.
3. **Listening test:** run the same assistant responses through all candidate providers. Include neutral explanation, empathetic response, mild humor, urgent instruction, and multi-sentence narrative.
4. **Continuity test:** synthesize a paragraph split into sentence chunks with and without Hume `context`. Listen for prosody jumps and measure latency overhead.
5. **Timestamp test:** request `include_timestamp_types: ["word"]` and verify whether returned timings are stable enough for read-along highlighting.
6. **Cost sanity check:** compute effective per-turn cost against representative Parley assistant responses, not just vendor list prices.

## Recommendation

Add Hume Octave 2 as a **premium expressive TTS candidate**, not as the default provider.

For near-term Parley development, Hume is best framed as:

- a listening-quality benchmark for persona voice;
- a future provider-native target for expression annotations;
- a candidate for word-level TTS highlighting because of Octave 2 timestamps;
- a provider to evaluate once the TTS trait grows beyond raw bytes and character counts.

I would prioritize xAI for low-cost default TTS, Gradium for low-latency full-duplex experiments, and Hume for the question: "Can Parley's AI voice sound emotionally intentional instead of merely readable?"
