# Hume Octave 2 TTS Integration

**Status:** Draft
**Type:** Specification
**Audience:** Both
**Date:** 2026-04-26

## 1. Overview

This spec adds Hume Octave 2 as an optional text-to-speech provider in Parley's existing conversation voice pipeline.

Hume is not the default TTS provider in this spec. It is added as a premium expressive voice candidate that can be compared against ElevenLabs, xAI, and Gradium using the same orchestrator, cache, and playback path. The first implementation uses Hume's HTTP streamed-file endpoint because Parley's current `TtsProvider` contract streams raw audio bytes. A later implementation should move to Hume's streamed-JSON endpoint to capture `generation_id`, word/phoneme timestamps, and other metadata.

Research background: [research/hume-octave-2-tts.md](research/hume-octave-2-tts.md).

Related specs:

- [conversation-voice-slice-spec.md](conversation-voice-slice-spec.md)
- [paragraph-tts-chunking-spec.md](paragraph-tts-chunking-spec.md)
- [expressive-annotation-spec.md](expressive-annotation-spec.md)
- [secrets-storage-spec.md](secrets-storage-spec.md)
- [xai-speech-integration-spec.md](xai-speech-integration-spec.md)

## 2. Position

Hume's best fit is emotionally intentional persona speech: coaching, roleplay, guided practice, narrative assistant responses, and any mode where delivery quality matters more than raw cost. Hume's Octave 2 preview latency and instant streaming make it plausible for interactive conversation, but the provider should earn default status through measured latency, listening tests, and cost review.

The integration should preserve Parley's provider philosophy: the orchestrator talks to a provider-neutral `TtsProvider`; Hume-specific fields stay inside `HumeTts` until the shared trait needs a real new capability.

## 3. Goals

- Add `ProviderId::Hume` as a TTS provider with `PARLEY_HUME_API_KEY` secret resolution.
- Add `proxy::tts::HumeTts` implementing the existing `TtsProvider` trait.
- Synthesize Octave 2 speech through Hume's proxy-side HTTP streaming API without exposing the API key to the WASM frontend.
- Reuse the existing conversation pipeline: `ChunkPlanner`, optional annotator pass, `TtsProvider`, `SilenceSplicer`, `FsTtsCache`, `TtsHub`, and `/conversation/tts/{turn_id}` SSE.
- Keep Hume disabled until a Hume credential and voice id are configured.
- Provide unit tests for registry wiring, request construction, streaming audio success, empty-text rejection, HTTP error handling, and cost calculation.
- Define the follow-up trait changes needed for Hume metadata, continuation, timestamps, and provider-native expression.

## 4. Non-Goals

- Making Hume the default TTS provider.
- Using Hume EVI or any unified speech-to-speech agent API. Parley keeps STT, LLM, and TTS as independently swappable stages.
- Voice cloning, voice conversion, or voice-design workflows. Those require explicit consent/provenance UX and are deferred.
- Browser-direct Hume WebSocket calls or short-lived Hume access-token issuance. The proxy owns the API key in this spec.
- Provider-native Hume acting instructions in v1. Octave 2 docs currently mark natural-language `description` acting instructions as coming soon; v1 sends plain text plus voice.
- Word/phoneme timestamp ingestion. Hume supports timestamps on Octave 2, but Parley's current TTS stream cannot carry metadata.

## 5. Requirements

| ID | Requirement | Verification |
| --- | --- | --- |
| HUME-01 | Hume appears in the provider registry as a TTS provider with env var `PARLEY_HUME_API_KEY`. | `providers` unit tests cover enum order, serialization, and registry metadata. |
| HUME-02 | The frontend never receives the Hume API key. | Code review plus existing secrets API tests; Hume calls originate only in the proxy. |
| HUME-03 | `HumeTts` rejects empty text before making an HTTP request. | Provider unit test with a mock server that would fail if called. |
| HUME-04 | `HumeTts` sends `version: "2"`, exactly one utterance, a configured voice, `instant_mode: true`, and `num_generations: 1`. | Provider unit test records the JSON request body. |
| HUME-05 | Successful streamed-file responses emit one or more `TtsChunk::Audio` frames followed by exactly one `TtsChunk::Done { characters }`. | Provider unit test using a streaming mock response. |
| HUME-06 | Non-2xx responses become `TtsError::Http { status, body }`. | Provider unit test for 401/422 response. |
| HUME-07 | Hume v1 does not claim ElevenLabs-style expressive tag support. | Unit test asserts `supports_expressive_tags() == false`. |
| HUME-08 | Hume audio is not spliced with mismatched MP3 silence. | Implementation either verifies Hume output matches `Mp3_44100_128` or updates the format/splicer model before enabling silence. |
| HUME-09 | Hume cost calculation uses the documented configured pricing tier, defaulting to Creator overage until pricing config exists. | Pure unit test for 1,000 characters. |
| HUME-10 | Hume implementation has no orchestrator behavior changes beyond provider selection and format-safe silence handling. | Orchestrator regression tests continue to pass with a mock provider. |

## 6. Architecture

```mermaid
flowchart LR
    LLM[LLM stream] --> Planner[ChunkPlanner]
    Planner --> Annotator[Expression annotator]
    Annotator --> Provider[HumeTts]
    Provider --> Splicer[Format-safe silence handling]
    Splicer --> Cache[FsTtsCache]
    Splicer --> Hub[TtsHub]
    Hub --> Browser[/conversation/tts/{turn_id} SSE]
    Secrets[SecretsManager] --> Provider
    Hume[Hume /v0/tts/stream/file] --> Provider
    Provider --> Hume
```

The first Hume slice should be boring by design: one provider module, one registry entry, one startup wiring path, and tests. Hume's advanced capabilities stay behind follow-up work until the shared TTS contract can represent them honestly.

### 6.1 Provider Registry

`proxy/src/providers.rs` gains one provider:

```rust
ProviderId::Hume

ProviderDescriptor {
    id: "hume",
    display_name: "Hume",
    category: ProviderCategory::Tts,
    env_var: "PARLEY_HUME_API_KEY",
}
```

If the xAI multi-category registry change lands first, Hume is a single `tts` row and does not need special category handling.

### 6.2 Module Layout

```text
proxy/
  src/
    tts/
      hume.rs        # Hume Octave 2 TtsProvider implementation
      mod.rs         # pub mod hume; re-export HumeTts
    providers.rs     # ProviderId::Hume + REGISTRY row
```

No `parley-core` changes are required for the streamed-file v1 path unless output-format negotiation forces an `AudioFormat` expansion.

### 6.3 Startup Wiring

Conversation setup currently resolves the active TTS provider from configured secrets and constructs an `Arc<dyn TtsProvider>`. Hume follows the ElevenLabs pattern:

1. Resolve `ProviderId::Hume` credential through `SecretsManager`.
2. If configured, construct `HumeTts::new(api_key, reqwest::Client)`.
3. Use the active persona/model voice id as the Hume voice reference.
4. If no Hume credential or voice id exists, return the existing provider-not-configured path.

Provider selection UI is not specified here because [xai-speech-integration-spec.md](xai-speech-integration-spec.md) already owns the generic TTS voice-selection work. Until that lands, the Hume spike can use the existing `tts_voice_id` path with a Hume voice id.

## 7. Hume API Contract

### 7.1 Authentication

The proxy authenticates REST calls with:

```text
X-Hume-Api-Key: <resolved key>
```

The key is resolved from `PARLEY_HUME_API_KEY` or the OS keystore per [secrets-storage-spec.md](secrets-storage-spec.md). Browser-direct access tokens are out of scope.

### 7.2 V1 Endpoint

```text
POST https://api.hume.ai/v0/tts/stream/file
Content-Type: application/json
Accept: audio/mpeg
```

V1 uses streamed file because it returns raw audio bytes and fits the existing `TtsChunk::Audio(Vec<u8>)` stream.

Request body shape:

```json
{
  "version": "2",
  "instant_mode": true,
  "num_generations": 1,
  "strip_headers": true,
  "utterances": [
    {
      "text": "Hello from Parley.",
      "voice": { "id": "<hume-voice-id>" }
    }
  ]
}
```

Rules:

- `version` is always `"2"` so the request uses Octave 2 rather than Hume's automatic routing.
- `voice` is required because Octave 2 rejects `version: "2"` requests without a voice.
- `instant_mode` stays enabled for conversational latency.
- `num_generations` stays `1` because the conversation path needs one audio result per chunk.
- `strip_headers` stays enabled so streamed chunks from one generation concatenate into one playable file.
- `description` is omitted in v1 because Octave 2 natural-language acting instructions are documented as coming soon.
- `speed` and `trailing_silence` are omitted in v1 unless a later profile field explicitly maps to them.
- `include_timestamp_types` is omitted in v1 because the streamed-file response cannot carry timestamp metadata through Parley's current `TtsChunk` shape.

### 7.3 Output Format Gate

Hume docs confirm MP3, WAV, and PCM output support, but the fetched API docs did not prove that Hume can be pinned to Parley's current `Mp3_44100_128` format. The implementation must not pretend the format matches.

The implementation must choose one of these paths before enabling Hume in normal playback:

| Path | Requirement | Recommendation |
| --- | --- | --- |
| A | Verify Hume can return 44.1 kHz, 128 kbps MP3 and request that format explicitly. | Best if supported; Hume can reuse `AudioFormat::Mp3_44100_128` and the current `SilenceSplicer`. |
| B | Add the actual Hume MP3 format to `AudioFormat` and add a matching checked-in silence frame. | Correct if Hume's MP3 sample rate or bitrate differs. |
| C | Disable silence splicing for Hume until the format model is expanded. | Acceptable for a spike only; do not ship as the default user path. |

`HumeTts::output_format()` may return `AudioFormat::Mp3_44100_128` only when path A is verified by documentation or by inspecting a live Hume response's MPEG headers.

## 8. Provider Behavior

### 8.1 `HumeTts`

`HumeTts` mirrors the ElevenLabs adapter shape:

```rust
pub struct HumeTts {
  api_key: Arc<str>,
  endpoint_base: Arc<str>,
  client: reqwest::Client,
  pricing: HumePricingTier,
}
```

Constructor requirements:

- `new(api_key, client)` uses `https://api.hume.ai/v0/tts`.
- `with_endpoint(api_key, endpoint_base, client)` exists for mock HTTP tests.
- Endpoint composition appends `/stream/file` for v1.

Trait behavior:

| Method | Hume v1 behavior |
| --- | --- |
| `id()` | Returns `"hume"`. |
| `synthesize(request, ctx)` | Rejects empty text, posts one Octave 2 streamed-file request, maps response body chunks to `TtsChunk::Audio`, then emits `Done`. |
| `output_format()` | Returns the verified format only; see [§7.3](#73-output-format-gate). |
| `supports_expressive_tags()` | Returns `false` in v1. Hume expression is not ElevenLabs tag syntax. |
| `cost(characters)` | Uses configured Hume tier rate, defaulting to Creator overage: `$0.15 / 1,000` characters. |

`SynthesisContext.previous_text`, `next_text_hint`, and `provider_state` are ignored in the streamed-file v1 path. They become useful after streamed JSON exposes `generation_id` continuation state.

### 8.2 Cost Model

Use a small enum or config value so the adapter's cost math is explicit:

```rust
pub enum HumePricingTier {
  Creator,
  Pro,
  Scale,
  Business,
}
```

Default rate:

| Tier | Overage price | Per-character cost |
| --- | --- | --- |
| Creator | `$0.15 / 1,000` chars | `0.000_15` |
| Pro | `$0.12 / 1,000` chars | `0.000_12` |
| Scale | `$0.10 / 1,000` chars | `0.000_10` |
| Business | `$0.05 / 1,000` chars | `0.000_05` |

Until provider pricing config exists, ship `Creator` as the default and document that cost reporting is an overage estimate, not an invoice reconciliation.

### 8.3 Error Handling

| Failure | Behavior |
| --- | --- |
| Empty text | Return `TtsError::Other("empty text")`; do not call Hume. |
| Transport error | Return `TtsError::Transport`. |
| HTTP non-success | Return `TtsError::Http { status, body }` with the best-effort response body. |
| Empty successful response | Emit `Done` after zero audio frames; tests should pin this behavior or treat it as protocol error. Recommendation: protocol error, because silent success hides provider problems. |
| Stream read error after partial audio | Return stream item `Err(TtsError::Transport)`; orchestrator preserves text and marks TTS failed, matching current behavior. |

## 9. Streamed JSON Follow-Up

The streamed-file adapter proves credentials, latency, and basic playback. It does not exploit Hume's strongest integration features. The next Hume-specific slice should switch to:

```text
POST https://api.hume.ai/v0/tts/stream/json
```

That slice needs shared TTS contract changes:

| Need | Contract change |
| --- | --- |
| Preserve Hume continuation | Add `ProviderContinuationState::HumeGenerationId(String)` and let providers return updated continuation state. |
| Carry word/phoneme timestamps | Add metadata-bearing TTS frames or a side channel, e.g. `TtsChunk::Timestamp(TtsTimestamp)`. |
| Capture generation/snippet ids | Add provider metadata to the terminal frame or provenance. |
| Decode base64 audio | Hume adapter parses streamed JSON audio objects and yields raw bytes to existing cache/playback. |
| Keep audio playable | Use `strip_headers: true` for concatenated container chunks. |

Recommended terminal-frame shape for the future:

```rust
pub enum TtsChunk {
  Audio(Vec<u8>),
  Metadata(TtsMetadata),
  Done {
    characters: u32,
    provider_state: Option<ProviderContinuationState>,
  },
}
```

This change should be made only when a second provider or Hume JSON work actually needs it. Do not widen the trait during the streamed-file spike.

## 10. Expression Strategy

Hume should force a cleanup of Parley's expression abstraction, but not in the first adapter.

The current `supports_expressive_tags() -> bool` answers a narrow question: can the provider accept ElevenLabs-style inline tags without reading them aloud? Hume's expressive controls are different:

- Octave infers delivery from text and voice context.
- Octave supports `speed` and `trailing_silence` across models.
- Hume documents `[pause]` and `[long pause]` text markers.
- Natural-language `description` acting instructions are Octave 1-only as of the current docs, with Octave 2 support coming.

Therefore Hume v1 returns `false` for `supports_expressive_tags()`. A later expression slice should replace the boolean with capability data:

```rust
pub struct TtsExpressionCapabilities {
  pub inline_tags: InlineTagDialect,
  pub style_prompt: StylePromptSupport,
  pub speed_control: bool,
  pub trailing_silence_control: bool,
  pub pause_tokens: bool,
}
```

Only after that exists should Parley's provider-neutral expression annotations target Hume's `description`, `speed`, `trailing_silence`, or pause syntax.

## 11. Voice Handling

### 11.1 V1

The first implementation accepts a Hume voice id through the existing `tts_voice_id` path. Hume also supports `{ name, provider }`, but using ids is less ambiguous and easier to validate.

V1 request voice shape:

```json
{ "voice": { "id": "<hume-voice-id>" } }
```

### 11.2 Voice List Follow-Up

When generic TTS voice selection lands, Hume should implement `voices()` against Hume's voice list endpoint and return provider-neutral descriptors. The descriptor should include enough metadata to distinguish Hume library voices from custom voices.

Voice cloning, voice conversion, and voice design are deferred until Parley has explicit consent capture and provenance fields for generated or cloned voices.

## 12. Implementation Plan

1. Verify Hume output format with either docs or a tiny live request. Decide [§7.3](#73-output-format-gate) path A, B, or C.
2. Add `ProviderId::Hume` and registry tests.
3. Add `proxy/src/tts/hume.rs` with constructor, endpoint override, request body construction, streamed-file response handling, and cost calculation.
4. Re-export `HumeTts` from `proxy/src/tts/mod.rs`.
5. Wire provider selection where ElevenLabs is currently constructed.
6. Add unit tests with `wiremock` matching the ElevenLabs adapter style.
7. Run `cargo fmt`, `cargo test -p parley-proxy`, and any workspace-level tests affected by provider registry changes.
8. Run a manual listening spike with the same text set used for ElevenLabs, xAI, and Gradium comparisons.

## 13. Test Plan

| Component | Tests |
| --- | --- |
| Provider registry | Hume variant order matches registry; `"hume"` parses and serializes; display name/category/env var match spec. |
| `HumeTts` request | Mock server records one POST to `/v0/tts/stream/file`; JSON includes `version: "2"`, `instant_mode: true`, `num_generations: 1`, `strip_headers: true`, text, and voice id. |
| `HumeTts` success | Mock audio response emits bytes; stream yields `Audio(bytes)` then `Done { characters }`. |
| `HumeTts` empty text | Empty text returns local error; mock server receives no request. |
| `HumeTts` HTTP error | 401 or 422 maps to `TtsError::Http` with status and body. |
| `HumeTts` cost | 1,000 characters at Creator tier equals `$0.15`. |
| Output format | If path A is used, a test or documented fixture asserts returned MP3 header matches `Mp3_44100_128`; if path B is used, silence-frame header tests mirror `silence.rs`. |
| Orchestrator | Existing mock-provider tests remain the main oracle; add Hume-specific orchestration only if provider selection code branches on Hume. |

## 14. Open Decisions

| ID | Question | Recommendation |
| --- | --- | --- |
| OQ-01 | Should Hume be selectable before generic TTS provider UI is finished? | Yes, behind config/manual voice id for a spike; polished UI waits for generic provider selection. |
| OQ-02 | Should streamed-file or streamed-JSON ship first? | Streamed-file first for integration safety; streamed-JSON second for Hume value. |
| OQ-03 | Should Hume v1 use `description` acting instructions anyway? | No. Do not target undocumented Octave 2 behavior. |
| OQ-04 | Should Hume become the default if it sounds best? | Not without cost and latency review. It is premium-priced and preview-labeled. |
| OQ-05 | Should Hume EVI be evaluated? | Separately. EVI collapses Parley's swappable STT/LLM/TTS pipeline and needs its own architecture discussion. |

## 15. Acceptance Checklist

- [ ] Hume credential can be configured through the existing secrets model.
- [ ] Hume can synthesize one assistant chunk through the proxy without exposing the API key.
- [ ] Hume audio plays through the existing SSE/cache media path.
- [ ] The implementation does not splice mismatched MP3 silence.
- [ ] Tests cover provider success, failure, request shape, registry wiring, and cost.
- [ ] Listening spike compares Hume against ElevenLabs, xAI, and Gradium.
- [ ] Follow-up issues/specs are created for streamed JSON metadata, timestamps, continuation, and expression capabilities.
