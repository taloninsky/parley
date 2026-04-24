# xAI Speech Integration — Specification

> Status: **Draft**
> Author: Gavin + BigDog
> Date: 2026-04-23

---

## 1. Overview

xAI shipped standalone Speech-to-Text (`grok-stt`) and Text-to-Speech APIs on **2026-04-18**, built on the same infrastructure that powers Grok Voice, Tesla, and Starlink customer support. Both APIs are REST + WebSocket, Bearer-authenticated, and share the same `api.x.ai` origin as the existing Grok chat surface. xAI publishes a WER of **5.0%** on phone-call entity recognition versus AssemblyAI **21.3%**, Deepgram **13.5%**, and ElevenLabs **12.0%**, and quotes **$0.10/hr batch**, **$0.20/hr streaming** STT and **$4.20 per 1M characters** TTS — materially cheaper than the incumbents parley already integrates.

This spec adds xAI as a first-class STT and TTS provider in the parley framework, alongside (not replacing) AssemblyAI and ElevenLabs. It lands squarely on [Philosophy §6 — *Models come and go*](philosophy.md) and plugs into the trait-based provider model already in place:

- **TTS end-to-end is already wired** via [`conversation-voice-slice-spec.md`](conversation-voice-slice-spec.md) (sentence-level chunking through `ChunkPlanner`, `TtsHub` broadcast, `FsTtsCache` on-disk cache, the sibling `/conversation/tts/{turn_id}` SSE, and the `Speaking` orchestrator state). ElevenLabs is the only provider today. xAI TTS lands as a second `TtsProvider` implementation that slots into that plumbing without orchestrator changes.
- **STT currently runs frontend-direct to AssemblyAI** via short-lived tokens proxied through the backend. xAI STT introduces the first *proxy-fronted* STT path — which is the point at which we formalize an `SttProvider` trait on the proxy side.
- **Voice selection UI does not exist yet**. [`conversation-voice-slice-spec.md` VS/Non-Goals](conversation-voice-slice-spec.md) explicitly defers per-persona voice override; this spec unblocks that by adding a voice picker that works across providers (ElevenLabs + xAI).

The docstring at [`proxy/src/orchestrator/mod.rs:7`](../proxy/src/orchestrator/mod.rs) calling the TTS slice "a later slice" is stale — the slice shipped; that comment and the "what's deferred" list in [`architecture.md`](architecture.md) around line 706 need an update as part of this work (see [§13.1](#131-in-scope)).

---

## 2. Goals

- xAI STT (`grok-stt`) is a selectable provider for both **streaming capture** (WebSocket) and **file transcription** (REST), feature-parity with the existing AssemblyAI integration.
- xAI TTS is a selectable provider implementing the `TtsProvider` trait defined in `proxy/src/tts/mod.rs`, with **voice selection**, **language selection**, and **output-format negotiation** exposed as first-class request fields.
- Users configure their xAI API token through the existing Settings surface (`/api/secrets/status`, `src/ui/secrets.rs`). The [secrets-storage spec](secrets-storage-spec.md) registry gains exactly one new `ProviderId::Xai` entry and picks up the UI automatically.
- A new **TTS voice-selection UX** (which did not previously exist — [see secrets-storage-spec.md §10.2](secrets-storage-spec.md)) lands as part of this work. It is generic across TTS providers; xAI is its first consumer.
- Every new outbound call path has unit tests with a mock HTTP backend. The audio pipeline (capture → STT → transport) is exercised against a local WebSocket fixture that mimics the documented xAI wire protocol.
- All API keys stay in the proxy keystore. The WASM frontend never sees the xAI bearer token at any point (same invariant as [secrets-storage-spec.md §4.5](secrets-storage-spec.md)).

## 3. Non-Goals

- **Orchestrator state-machine changes.** The `Speaking` state, `TtsStarted`/`TtsSentenceDone`/`TtsFinished` events, `TtsTurn` scratch state, cache, and broadcast hub already exist per [conversation-voice-slice-spec.md](conversation-voice-slice-spec.md). xAI TTS reuses them verbatim.
- **Barge-in / VAD.** Already marked deferred in [conversation-voice-slice-spec.md §2.2](conversation-voice-slice-spec.md). xAI does not change the calculus.
- **Changing the TTS dispatch shape** from per-sentence HTTP to xAI's `wss://api.x.ai/v1/tts` streaming socket. xAI TTS is wired via its unary `POST /v1/tts` endpoint in v1, matching the per-sentence HTTP pattern ElevenLabs uses today (VS-3 in [conversation-voice-slice-spec.md §2.4](conversation-voice-slice-spec.md)). WS TTS is a follow-up — see [§12.1](#121-xai-tts-websocket-streaming).
- **xAI Voice Agent API** (`wss://api.x.ai/v1/realtime` — unified STT+LLM+TTS socket). Out of scope. Evaluated against parley's pipeline philosophy in [§12.2](#122-voice-agent-api) and declined for v1. *[Pending user confirmation — see OQ-03.]*
- **Deprecating AssemblyAI or ElevenLabs.** Both remain fully supported. A future spec may revisit default selection. See OQ-01.
- **Cost aggregation at the session level.** Per-call cost *capture* for STT minutes and TTS characters is in scope (see [§7](#7-cost-accounting)); roll-up display mirrors the deferred LLM cost-aggregation note at [architecture.md line 719](architecture.md).
- **Real-time beamforming / spatial diarization** of xAI STT output. xAI returns word-level `speaker` indexes under `diarize=true`; the spatial coordinate system ([architecture.md §"Spatial Coordinate System"](architecture.md)) is upstream of STT and unaffected.
- **Migration of existing recordings.** Session provenance already pins the provider per session ([architecture.md §"Provenance"](architecture.md)); older sessions keep their AssemblyAI / ElevenLabs artifacts.

---

## 4. Open Questions (Blocking)

These must be resolved before implementation begins. Each carries BigDog's recommendation with rationale; Gavin's explicit decision is required.

### OQ-01 — Default provider selection

**Question:** On fresh install / unconfigured state, which STT and TTS provider should the default profile point to?

**Options:**
- **(a)** Keep AssemblyAI (STT) and ElevenLabs (TTS) as defaults. xAI is opt-in.
- **(b)** Flip defaults to xAI. AssemblyAI / ElevenLabs remain available.
- **(c)** No default — installer forces the user to pick after seeing a comparison card.

**Recommendation: (a) for v1.** Rationale: xAI's WER numbers are vendor-published and not yet corroborated by an independent benchmark; we have months of operational data on AssemblyAI; default-switching is a one-line change once we've validated xAI against real parley audio. A decision bias toward *stability for the default path, visibility for the new option*. Revisit after 30 days of xAI usage across at least two real profiles.

### OQ-02 — STT streaming connection topology

**Question:** When the browser captures audio for xAI streaming STT, does the WebSocket go **(a) browser → proxy → xAI** or **(b) browser → xAI directly** (with a short-lived token fetched from the proxy, the way AssemblyAI works today)?

**Options with consequences:**
- **(a) Proxy-through:** Simpler security posture — the xAI bearer token never leaves the proxy. Adds a hop of latency (~5–15 ms on localhost). Adds a new axum WebSocket route that shuttles binary frames bidirectionally. Matches how ElevenLabs TTS is already proxied.
- **(b) Direct with token exchange:** Lower latency, architecturally consistent with AssemblyAI. Requires xAI to offer a short-lived/scoped token endpoint — **this is not documented** in `docs.x.ai` as of 2026-04-23 and must be verified with xAI support before committing. If xAI bearer tokens are long-lived API keys with no temp-token flow, (b) is off the table because exposing the raw key to the browser violates the invariant in [§2](#2-goals) and [secrets-storage-spec.md](secrets-storage-spec.md).

**Recommendation: (a) Proxy-through for v1.** Rationale: The token-exchange mechanism for xAI is unverified; blocking on vendor clarification is schedule risk; localhost latency overhead is in the noise next to 100–300 ms STT roundtrip to xAI itself. Switch to (b) in a follow-up if and only if xAI exposes a temp-token flow — leave the STT provider trait's `stream()` method signature compatible with both topologies. The AssemblyAI integration stays on (b) because it was designed around AssemblyAI's documented token endpoint.

### OQ-03 — Voice Agent API exposure

**Question:** Should parley's architecture explicitly accommodate xAI's unified Voice Agent API (`wss://api.x.ai/v1/realtime`), which collapses STT + LLM + TTS into one socket?

**Options:**
- **(a)** Ignore the Voice Agent API. Treat xAI STT and TTS as three independent providers the pipeline stitches together.
- **(b)** Add a new pipeline mode / orchestrator variant that delegates the whole turn to a unified provider, bypassing parley's STT→LLM→TTS stages when that provider is selected.

**Recommendation: (a) for v1.** Rationale: Philosophy §6 ("The core owns the pipeline; providers are pluggable") is explicit that the pipeline is parley's value, not the provider's. A unified vendor socket bypasses diarization, spatial, word-graph ingest, provenance, and vocabulary — all the things that make parley *parley*. Revisit only if latency in stage-composed mode is demonstrably worse than parley's targets, which we have no evidence of yet.

### OQ-04 — Coexistence trajectory

**Question:** Is xAI intended to coexist with AssemblyAI / ElevenLabs permanently, or is the long-term plan consolidation to one STT and one TTS vendor?

**Options:**
- **(a)** Permanent coexistence. Users may mix providers across profiles.
- **(b)** xAI replaces incumbents after a 30/60/90-day evaluation, via a deprecation path.
- **(c)** Decision deferred; no consolidation plan documented.

**Recommendation: (a).** Rationale: Philosophy §6 codifies provider pluralism as a non-negotiable. Removing providers we already support contracts the user's flexibility without benefit. The *default* may shift (OQ-01); the *set* should only grow.

### OQ-06 — Diarization and multichannel surface area

**Question:** xAI STT supports diarization (`diarize=true`) and multichannel (2–8 channels). Parley's philosophy §3 ("Audio deserves respect") strongly favors preserving channel information. How much of this do we expose in v1?

**Options:**
- **(a) MVP surface.** Send mono-mix like today's AssemblyAI path. Ignore `diarize` and `multichannel` request fields. STT returns a single speaker lane.
- **(b) Diarization on, multichannel off.** Enable `diarize=true` by default; let xAI label `speaker` indexes; project them into the word graph's `speaker_lane` field. Still send mono-mix.
- **(c) Full.** Enable both `diarize` and `multichannel` when the capture device supports it. Channels flow through unmixed; xAI runs per-channel STT.

**Recommendation: (b).** Rationale: Diarization is a *pure win* — xAI already does it, our data model already supports lanes ([architecture.md §"Lane"](architecture.md)), and we set a better-than-AssemblyAI baseline. Multichannel needs the upstream capture pipeline (cpal, multi-channel storage) which is a bigger piece not yet in place; don't block diarization on it.

### OQ-07 — Speech-tag affordance

**Question:** xAI TTS supports inline speech tags (`[laugh]`, `[sigh]`, `[pause]`) and wrapping tags (`<whisper>`, `<slow>`, `<singing>`). How does the assistant's generated text feed these in? Three places they might come from: (1) the LLM generates them directly, (2) the persona system-prompt instructs the LLM to use them, (3) a dedicated post-LLM formatter inserts them. Does v1 support any of these?

**Options:**
- **(a)** No tag support. Plain text to TTS. Tags in assistant output are passed through verbatim (xAI renders them; ElevenLabs would spell them).
- **(b)** Pass-through only. Persona prompts may instruct the LLM to use tags; we don't transform the text. The [expressive-annotation-spec](expressive-annotation-spec.md) already exists — reference it, don't re-specify.
- **(c)** Tag-aware voice-composer. We add a post-LLM stage that parses tags and emits them as `TtsRequest.speech_tags` structured fields. Out of scope for v1.

**Recommendation: (b).** Rationale: [`expressive-annotation-spec.md`](expressive-annotation-spec.md) already covers this shape — align with that spec, don't duplicate. The xAI `TtsRequest` accepts tags inline in `text`, so pass-through works with zero transformation.

### OQ-08 — Per-call cost capture

**Question:** The orchestrator already captures LLM cost per turn in `TurnProvenance` ([architecture.md Conversation orchestrator §"Cost"](architecture.md)). Do we capture STT-minute and TTS-character cost per turn too?

**Recommendation: Yes, but minimal.** Add `stt_cost_usd: Option<f64>` and `tts_cost_usd: Option<f64>` to `TurnProvenance`. The `TtsProvider::cost()` method already exists; define an analogous `SttProvider::cost()`. Aggregation / display remains deferred (same as LLM — see [architecture.md line 719](architecture.md)).

---

## 5. xAI API Reference (as used by this spec)

This section is the precise contract the proxy will implement. Everything below is verified against `docs.x.ai` on 2026-04-23.

### 5.1 Authentication

All endpoints:
```
Authorization: Bearer <XAI_API_KEY>
```

Resolved by the proxy via `SecretsManager::resolve(ProviderId::Xai, credential)` — exactly the pattern from [secrets-storage-spec.md §4.3](secrets-storage-spec.md).

### 5.2 STT — REST (batch / file)

```
POST https://api.x.ai/v1/stt
Content-Type: multipart/form-data
```

| Field | Type | Required | Notes |
|---|---|---|---|
| `file` | binary | one-of | Audio file; max **500 MB**. Formats: wav, mp3, ogg, opus, flac, aac, mp4, m4a, mkv, pcm, µ-law, a-law. |
| `url` | string | one-of | Server-side download URL. |
| `model` | string | yes | `grok-stt`. |
| `language` | string | optional | ISO 639-1 (e.g. `en`). Required for `format=true`. |
| `format` | `"true"`/`"false"` | optional | Inverse text normalization ("one twenty three" → "$123"). |
| `diarize` | `"true"`/`"false"` | optional | Per-word speaker index. See OQ-06. |
| `multichannel` | `"true"`/`"false"` | optional | Per-channel transcription. See OQ-06. |
| `channels` | integer | conditional | 2–8, required if `multichannel=true` on raw audio. |
| `audio_format` | enum | conditional | Hint for raw (`pcm`/`mulaw`/`alaw`). |
| `sample_rate` | string | conditional | Required for raw. 8000/16000/22050/24000/44100/48000. |

Response body (`application/json`):
```json
{
  "text": "…",
  "language": "",
  "duration": 8.4,
  "words": [
    { "text": "hello", "start": 0.0, "end": 0.24, "confidence": 0.95, "speaker": 0 }
  ],
  "channels": []
}
```

### 5.3 STT — WebSocket (streaming)

```
wss://api.x.ai/v1/stt
```

**Protocol** (verified at docs level; precise event-type strings must be confirmed during impl — see [§10.1](#101-spike-verify-websocket-protocol)):

- Client → Server: raw binary audio frames (format negotiated at handshake via query params).
- Client → Server (close): text frame `{"type":"audio.done"}`.
- Server → Client: interim and final transcript events. Event type names (`transcript.delta`, `transcript.final`, or similar) are not explicitly enumerated in the public docs; treat them as **unverified** and add an integration spike ([§10.1](#101-spike-verify-websocket-protocol)) as the first implementation task.

**Limits:** 600 RPM, 10 RPS, 100 concurrent streams per team.

### 5.4 TTS — REST (unary)

```
POST https://api.x.ai/v1/tts
Content-Type: application/json
```

Request body:
```json
{
  "text": "Hello! Welcome to parley.",
  "voice_id": "eve",
  "language": "en",
  "output_format": {
    "codec": "mp3",
    "sample_rate": 24000,
    "bit_rate": 128000
  },
  "optimize_streaming_latency": 0,
  "text_normalization": true
}
```

- `text`: ≤ 15,000 characters.
- `voice_id`: `eve` | `ara` | `rex` | `sal` | `leo` (default `eve`). Fetch via `GET /v1/tts/voices` for canonical list.
- `language`: BCP-47 or `auto`.
- `codec`: `mp3` | `wav` | `pcm` | `mulaw` | `alaw`.
- `sample_rate`: 8000 / 16000 / 22050 / **24000 (default)** / 44100 / 48000.
- `bit_rate` (mp3 only): 32000 / 64000 / 96000 / **128000 (default)** / 192000.

Response: raw audio bytes with the codec's MIME type.

### 5.5 TTS — WebSocket (streaming)

```
wss://api.x.ai/v1/tts?language=en&voice=eve&codec=mp3&sample_rate=24000&bit_rate=128000
```

- Client → Server: `{"type":"text.delta","delta":"…"}` (repeated), then `{"type":"text.done"}`.
- Server → Client: `{"type":"audio.delta","delta":"<base64>"}` (repeated), then `{"type":"audio.done","trace_id":"<uuid>"}`.

**Limits:** 50 concurrent WS sessions per team, 15 min session timeout.

### 5.6 Voices catalog

```
GET https://api.x.ai/v1/tts/voices
```

Returns the list of available voice IDs with metadata (exact schema not documented; treat as `Vec<{id: String, ...}>` and only depend on `id` until the schema stabilizes). Cached by the proxy with a TTL of 24 h.

---

## 6. Architecture Changes

### 6.1 Provider Registry

`proxy/src/providers.rs` gains:

```rust
// ProviderId enum — one new variant
Xai,

// REGISTRY entry
ProviderDescriptor {
    id: ProviderId::Xai,
    category: ProviderCategory::Stt, // see §6.1.1
    display_name: "xAI",
    env_var: "PARLEY_XAI_API_KEY",
    service_prefix: "parley",
    account_prefix: "xai",
}
```

#### 6.1.1 Multi-category providers

The current registry enforces one-category-per-provider. xAI is both `Stt` and `Tts`. Three options:

- **(a)** Add `ProviderCategory::Multi(Vec<ProviderCategory>)`.
- **(b)** Let one `ProviderId` appear in multiple registry rows, one per category. No enum change needed; key becomes `(id, category)`.
- **(c)** Declare xAI twice in the enum: `XaiStt` and `XaiTts`. Cleanest from a type perspective; duplicates the display name.

**Recommendation: (b).** Minimal surface change; retains one credential per provider which is correct (one bearer token serves both APIs).

The `REGISTRY` becomes `&'static [ProviderDescriptor]` where a single provider id can appear with multiple category rows. The `/api/secrets/status` shape stays identical (xAI appears under both `stt` and `tts` arrays with the same credential list).

### 6.2 STT Provider Trait

`proxy/src/stt/mod.rs` does not currently exist as a trait — AssemblyAI's integration is frontend-direct. This spec introduces it:

```rust
#[async_trait]
pub trait SttProvider: Send + Sync {
    fn id(&self) -> ProviderId;

    /// File / batch transcription.
    async fn transcribe(&self, req: SttRequest) -> Result<Transcript, SttError>;

    /// Streaming transcription. Returns a pair of channels:
    ///   tx_audio: sink for PCM16 LE binary frames
    ///   rx_events: stream of TranscriptEvent (interim/final)
    async fn stream(&self, config: SttStreamConfig) -> Result<SttStreamHandle, SttError>;

    /// Cost per second of input audio at this provider's current rates.
    fn cost_per_second(&self, streaming: bool) -> f64;
}
```

`SttRequest`, `Transcript`, `TranscriptEvent`, `SttStreamConfig`, `SttStreamHandle` are defined in `parley-core::stt` (WASM-clean — they travel over the HTTP/WS boundary).

### 6.3 TTS Provider Extension

`TtsProvider` exists at [`proxy/src/tts/mod.rs`](../proxy/src/tts/mod.rs) and is fully consumed by the orchestrator (`OrchestratorContext.tts`, the `Speaking` state, `TtsTurn`, `ChunkPlanner`, `FsTtsCache`, `TtsHub`). The xAI impl slots in unchanged:

- `id()` → `"xai"`.
- `output_format()` → `AudioFormat::Mp3_44100_128` in v1 (we request `sample_rate=44100, bit_rate=128000` on `POST /v1/tts` so the existing `SilenceSplicer` can splice matching silence). Other codecs are available from xAI but not wired until the format enum expands.
- `supports_expressive_tags()` → `true`. xAI renders `[laugh]`, `<whisper>`, etc. natively ([§5.4](#54-tts--rest-unary)).
- `cost(characters)` → `characters * 4.20 / 1_000_000.0` USD.
- `synthesize(TtsRequest, SynthesisContext)` → unary `POST /v1/tts` per chunk. `SynthesisContext.previous_text` is ignored in v1 (xAI's REST endpoint has no documented prosody-continuity hint); `provider_state` is `None`. WS streaming with cross-chunk continuity is a follow-up — see [§12.1](#121-xai-tts-websocket-streaming).

The trait gains one new method for voice enumeration:

```rust
/// List voices supported by this provider. Default: empty.
async fn voices(&self) -> Result<Vec<VoiceDescriptor>, TtsError> {
    Ok(Vec::new())
}
```

`VoiceDescriptor { id: String, display_name: String, language_tags: Vec<String> }` lives in `parley-core::tts`.

ElevenLabs keeps returning `Ok(vec![])` until a separate change adds the voices endpoint there.

### 6.4 New Modules

```
proxy/src/stt/
├── mod.rs                    # SttProvider trait + shared types
├── assemblyai.rs             # Moved from src/stt/assemblyai.rs (see §6.5)
└── xai.rs                    # NEW — grok-stt REST + WS client

proxy/src/tts/
├── mod.rs                    # existing
├── elevenlabs.rs             # existing
└── xai.rs                    # NEW — xAI TTS client

parley-core/src/
├── stt.rs                    # NEW — SttRequest, Transcript, TranscriptEvent, VoiceDescriptor types
└── tts.rs                    # NEW — TtsRequest canonicalization (currently scattered in proxy)
```

### 6.5 AssemblyAI Relocation

Today `src/stt/assemblyai.rs` is in the **WASM frontend** and talks directly to AssemblyAI with a browser-side token. Under OQ-02(a), the xAI STT path is proxy-fronted. The AssemblyAI direct-connect path stays on the frontend (it works, and AssemblyAI's token flow is documented), but the `SttProvider` trait lives in the proxy.

This creates two provider-implementation locations:
- **Frontend provider** (`src/stt/assemblyai.rs`) for direct-connect providers with temp-token flows.
- **Proxy provider** (`proxy/src/stt/xai.rs`) for providers we proxy.

The `SttProvider` trait defined in §6.2 is a **proxy-side** trait. Frontend providers are not required to implement it. The proxy's HTTP surface (§7) exposes a uniform "STT stream" endpoint for proxy-side providers; the frontend selects between direct-connect and proxy-mediated flows based on a `stream_mode` field on the provider descriptor.

*BigDog note for Gavin: this is the seam where "coexistence" (OQ-04) gets structurally real. If we ever consolidate, collapsing the frontend STT module is trivial; keeping two modes forever is also cheap. Don't overbuild; just make the dispatch explicit.*

---

## 7. Cost Accounting

`TurnProvenance` gains:

```rust
pub struct TurnProvenance {
    // existing fields…
    pub stt_cost: Option<Cost>,
    pub tts_cost: Option<Cost>,
}
```

Populated at turn append time from `SttProvider::cost_per_second * duration` and `TtsProvider::cost(char_count)`. When a turn's STT or TTS went through a different provider than the assistant's LLM (the normal case), each is captured independently.

Aggregation / display is out of scope (same deferral as LLM — [architecture.md line 719](architecture.md)).

---

## 8. Proxy HTTP Surface

All routes on the existing proxy, localhost-bound. Auth is the same trust model as [secrets-storage-spec.md §5.6](secrets-storage-spec.md) (deferred channel binding tracked in that spec's §10.1).

### 8.1 `POST /api/stt/transcribe`

Body (JSON):
```json
{
  "provider": "xai",
  "credential": "default",
  "config": {
    "model": "grok-stt",
    "language": "en",
    "diarize": true,
    "format": true
  },
  "audio": { "source": "url", "url": "https://…" }
  // or { "source": "inline_base64", "data": "…", "audio_format": "wav" }
}
```

Response: `Transcript` JSON (same shape as [§5.2](#52-stt--rest-batch--file) response body, canonicalized).

### 8.2 `GET /api/stt/stream` — WebSocket upgrade

Query: `provider=xai&credential=default&language=en&diarize=true&sample_rate=16000`

Frames:
- Client → Server (binary): PCM16 LE audio.
- Client → Server (text): `{"type":"audio.done"}` to close.
- Server → Client (text): `{"type":"transcript.delta","words":[…]}` / `{"type":"transcript.final","text":"…","words":[…]}` / `{"type":"error","message":"…"}`.

The server-side WS handler is a bidirectional bridge: it owns the xAI WS connection, injects the bearer token from `SecretsManager::resolve(ProviderId::Xai, credential)`, translates PCM16 frames into whatever xAI expects at the binary boundary, and normalizes xAI's event names into parley's canonical `transcript.delta` / `transcript.final` before relaying to the browser.

### 8.3 `POST /api/tts/synthesize`

Body:
```json
{
  "provider": "xai",
  "credential": "default",
  "voice_id": "eve",
  "language": "en",
  "text": "…",
  "output_format": { "codec": "mp3", "sample_rate": 24000, "bit_rate": 128000 }
}
```

Response: audio bytes with appropriate `Content-Type`.

### 8.4 `GET /api/tts/stream` — WebSocket

Bidirectional bridge analogous to §8.2, for xAI TTS streaming. Client sends `text.delta` / `text.done`; server sends `audio.delta` / `audio.done`. Frontend consumes audio chunks and pipes them into Media Source Extensions for continuous playback.

### 8.5 `GET /api/tts/voices?provider=xai`

Returns `{ "voices": [{ "id": "eve", "display_name": "Eve", "language_tags": ["en","es-MX"] }, …] }`. Cached server-side per §5.6.

### 8.6 Failure mapping

- Provider key unresolved → `412 Precondition Failed` with `{error: "provider_not_configured", provider, credential}` — same shape as [secrets-storage-spec.md §5.5](secrets-storage-spec.md).
- xAI 429 → surfaced as `429` with `Retry-After` header preserved.
- xAI 5xx or timeout → `502 Bad Gateway` with `{error: "upstream_error", provider, detail}` (no raw upstream body to avoid leaking internal identifiers).
- WS drop during stream → server emits `{"type":"error","code":"upstream_disconnected"}` on the client socket, then closes.

---

## 9. UI Changes

### 9.1 Provider / Voice Selection in Profiles

Profiles ([architecture.md §"Profile"](architecture.md)) gain:

```toml
[stt]
provider = "xai"           # "xai" | "assemblyai"
credential = "default"
language = "en"
diarize = true

[tts]
provider = "xai"           # "xai" | "elevenlabs"
credential = "default"
voice_id = "eve"
language = "en"
codec = "mp3"
```

### 9.2 New Voice Picker Component

`src/ui/voice_picker.rs` (new). Generic across TTS providers. Fetches `/api/tts/voices?provider={id}` on mount; renders a dropdown with the voice `display_name`; stores selection in the active profile signal.

Used in:
- Settings → Profile editor (future — when profile CRUD UI lands).
- Conversation view's active-persona panel (immediately — lets the user swap voice mid-session for the next assistant turn).

### 9.3 Settings Panel — zero direct changes

The secrets panel auto-picks up the new `ProviderId::Xai` via the registry ([secrets-storage-spec.md §6.1](secrets-storage-spec.md)). xAI appears under both `STT` and `LLM`-adjacent sections because of §6.1.1(b). No UI code change required for the secrets surface.

*BigDog note: this is the payoff of the categorized registry. Verify the first-render does not show xAI as two duplicate cards — it should show as one card tagged with both categories, or two cards with the same credential state. Whichever, decide before shipping.*

### 9.4 Cookie path already removed

Already handled by [secrets-storage-spec.md §6.3](secrets-storage-spec.md). No new removals needed.

---

## 10. Implementation Plan — Spikes First

Per BigDog's principle of requirements before implementation, and given the xAI WS protocol is not fully documented, the first tasks are **verification spikes**, not production code.

### 10.1 SPIKE: Verify WebSocket Protocol

**Inputs:** a working xAI API key, a short test audio clip, a one-file Rust WS client using `tokio-tungstenite`.

**Deliverable:** a one-page markdown in `docs/research/xai-ws-protocol.md` enumerating:
- Exact event type strings xAI sends on the STT socket (interim and final transcript deltas).
- Exact binary frame format xAI expects (raw PCM? WAV? Opus? sample rate expectations?).
- Behavior on `audio.done`: clean close or half-close?
- TTS socket: does `audio.delta` base64 need padding fix-ups? What's the last-frame signal?

**Acceptance:** one round-trip of each socket with captured wire logs.

### 10.2 Implementation Task Order (post-spike)

1. `proxy/src/providers.rs` — add `Xai` variant + two registry entries (§6.1).
2. `parley-core/src/stt.rs`, `parley-core/src/tts.rs` — canonical types (§6.2, §6.3).
3. `proxy/src/stt/mod.rs` + `proxy/src/stt/xai.rs` — REST client first, WS second. Unit-tested against a local `wiremock` HTTP fixture and a small `tokio-tungstenite` echo fixture.
4. `proxy/src/tts/xai.rs` — REST then WS.
5. `proxy/src/stt_api.rs`, `proxy/src/tts_api.rs` — HTTP handlers from §8.
6. `parley-core/src/tts.rs` — extend `TtsProvider::voices()`.
7. `src/ui/voice_picker.rs` — component, wired into conversation view.
8. Profile schema extension (§9.1).
9. Provenance extension (§7).

Each step lands as its own commit with tests green before the next starts.

---

## 11. Test Plan

### 11.1 Unit Tests

**`proxy::stt::xai`:**
- REST request construction — multipart field names match [§5.2](#52-stt--rest-batch--file) exactly.
- REST response deserialization — round-trip against canned `application/json` responses covering: single-speaker, diarized multi-speaker, multichannel, empty `words`, `language: ""` edge case.
- Error classification — 400 → `SttError::InvalidRequest`, 401 → `SttError::Unauthorized`, 429 → `SttError::RateLimited { retry_after }`, 5xx → `SttError::Upstream`.
- Cost calculation — `cost_per_second(streaming=false) == 0.10 / 3600`; `cost_per_second(streaming=true) == 0.20 / 3600`.

**`proxy::tts::xai`:**
- Request body serialization — field order / defaults / `output_format` nesting.
- Voice list parsing + caching — expiry, stale-on-error fallback.
- `cost(char_count)` == `char_count as f64 * 4.20 / 1_000_000.0`.
- Speech-tag pass-through — text containing `[laugh]`, `<whisper>…</whisper>` is forwarded verbatim (no sanitization).

**`proxy::providers::registry`:**
- Multi-category provider listing: `xai` appears under both `Stt` and `Tts` buckets; credential list is identical in both views.
- Adding a new `ProviderCategory::X` does not break existing single-category providers.

**`parley_core::stt` / `parley_core::tts`:**
- Serde round-trips for every wire type (`SttRequest`, `Transcript`, `TranscriptEvent`, `TtsRequest`, `VoiceDescriptor`).
- WASM-clean: types compile with `wasm32-unknown-unknown` target in CI (same gate as `parley-core` today).

### 11.2 HTTP-Layer Tests

Using the existing proxy test harness (`tower::ServiceExt::oneshot`, in-memory `KeyStore`, mock xAI backend via `wiremock`):

- `POST /api/stt/transcribe` with `provider=xai, credential=default` when no key is configured → `412 provider_not_configured`.
- `POST /api/stt/transcribe` round-trips a canned response through the full multipart → JSON pipeline.
- `GET /api/stt/stream` WebSocket upgrade succeeds with valid query params; PCM16 frames bridged to mock xAI backend; mock's `transcript.delta` events relayed back with canonicalized type names.
- `POST /api/tts/synthesize` returns `audio/mpeg` body matching the mock's bytes verbatim.
- `GET /api/tts/voices?provider=xai` caches response (second call does not hit upstream).
- Upstream 5xx → proxy returns `502 upstream_error` and does not leak upstream response body.
- Upstream 429 → `Retry-After` preserved.
- WS mid-stream disconnect → error frame + clean close.

### 11.3 Manual Verification

Documented checklist for Gavin to run once, against the live xAI API with a real key:

1. Set `XAI_API_KEY` via the Settings panel. Confirm the keystore entry `xai/default` exists (Windows Credential Manager / Keychain).
2. Select a short audio file (≤30s) via the file-processing flow, provider=xai. Transcript returns within 5s and contains expected words with word-level timestamps.
3. Same file with `diarize=true`. If the clip has two speakers, `speaker` indexes are populated on word objects.
4. Start a live capture session, provider=xai STT streaming. Speak a few sentences. Partial transcripts appear in <1s; final transcript within 500ms of silence.
5. Request TTS synthesis of a 200-char sentence with `voice_id=eve`. MP3 plays back in browser; audio is intelligible.
6. Same with `voice_id=rex`, then `voice_id=ara`. Distinct voice characteristics audible.
7. TTS with inline tag `"Hello [laugh] this is a test"` — laugh is audible at the tag position.
8. TTS streaming — text is fed via WS in 3 chunks; audio plays back without gaps across chunk boundaries.
9. Disable the xAI credential. `/api/stt/transcribe` returns 412. UI shows "configure your xAI key" banner.
10. Re-enable. Session continues.
11. Deliberately send a malformed audio blob. Proxy returns a 400 with `{error: "invalid_audio"}`, not a 502.
12. Latency comparison — record 30s of speech twice, once on AssemblyAI, once on xAI. Note WER qualitatively (matches expectations or doesn't).

---

## 12. Follow-Ups (Separate Specs)

### 12.1 xAI TTS WebSocket Streaming

Today both ElevenLabs and (per this spec) xAI TTS dispatch via unary HTTP per sentence, matching VS-3 in [conversation-voice-slice-spec.md §2.4](conversation-voice-slice-spec.md). xAI publishes a streaming WS endpoint ([§5.5](#55-tts--websocket-streaming)) that accepts `text.delta` frames and returns `audio.delta` frames — a different dispatch shape than the per-sentence loop the `ChunkPlanner` drives. Evaluating this means either:
- Extending the `TtsProvider` trait with an optional `synthesize_stream(text_stream)` method, or
- Swapping the orchestrator's dispatch loop to route whole-turn text directly to the provider when the provider opts in.

Both are orchestrator-shape changes, not provider-shape changes, and therefore out of scope for this spec. A `ProviderContinuationState::XaiTts(...)` variant would likely fall out of that work.

### 12.2 Voice Agent API

If evidence emerges that stage-composed latency (STT→LLM→TTS) is materially worse than xAI's unified `/v1/realtime` socket, evaluate a pipeline mode that delegates the whole turn. Per OQ-03, this is declined for v1.

### 12.3 STT Direct-Connect for xAI

If xAI exposes a short-lived-token endpoint analogous to AssemblyAI, move STT streaming from proxy-through to direct-from-browser. The `SttProvider` trait's stream-config surface is intentionally compatible with both topologies.

### 12.4 Multichannel Capture Pipeline

`cpal`-backed multi-channel capture ([architecture.md key crates](architecture.md)) feeding per-channel STT via xAI's `multichannel=true`. Blocked on OQ-06 and on the capture pipeline itself.

---

## 13. Scope

### 13.1 In Scope

- `proxy/src/providers.rs` — add `ProviderId::Xai`; accommodate multi-category (§6.1.1).
- `parley-core/src/{stt,tts}.rs` — canonical wire types.
- `proxy/src/stt/mod.rs` + `proxy/src/stt/xai.rs` — REST and WS client.
- `proxy/src/tts/xai.rs` — REST and WS client.
- `proxy/src/stt_api.rs`, `proxy/src/tts_api.rs` — HTTP/WS handlers in §8.
- `TtsProvider::voices()` method on the existing trait; xAI implements, ElevenLabs stubs to empty.
- `src/ui/voice_picker.rs` — component.
- Profile schema extension (§9.1).
- `TurnProvenance.stt_cost` / `tts_cost` fields (§7).
- The verification spike (§10.1) — docs/research/xai-ws-protocol.md.
- Unit tests (§11.1), HTTP-layer tests (§11.2), manual verification checklist (§11.3).
- `docs/architecture.md` update: new proxy modules, new HTTP routes, provider categorization notes.
- **Stale-docs fix**: the "later slice" docstring at [`proxy/src/orchestrator/mod.rs:7`](../proxy/src/orchestrator/mod.rs) and the deferred-TTS-slice callouts in [`architecture.md`](architecture.md) around line 706 are both obsolete (the voice slice has shipped). Correct them as part of landing this spec.

### 13.2 Out of Scope (Deferred / Tracked)

- xAI TTS WebSocket streaming dispatch (§12.1). v1 routes xAI TTS through the existing per-sentence HTTP loop the orchestrator already drives for ElevenLabs.
- Voice Agent API (§12.2).
- STT direct-connect for xAI (§12.3).
- Multichannel capture pipeline (§12.4).
- Default-provider switch (OQ-01) — not before 30-day evaluation.
- AssemblyAI / ElevenLabs deprecation.
- Profile CRUD UI — voice picker ships, but full profile editor is its own work.
- Session-level cost aggregation.

---

## 14. Resolved Decisions

Captured here as this spec is drafted and reviewed; moves up into the spec body as follow-up revisions accept them.

- *(none yet — pending resolution of §4 Open Questions.)*

---

## 15. Questions for Gavin

These need answers before implementation can begin. All are cross-references to [§4](#4-open-questions-blocking):

1. **OQ-01** — Default provider on fresh install?
2. **OQ-02** — Proxy-through vs direct-from-browser for xAI STT streaming?
3. **OQ-03** — Voice Agent API — truly out of scope?
4. **OQ-04** — Is permanent coexistence with AssemblyAI/ElevenLabs the long-term plan?
5. **OQ-06** — Diarization on by default? Multichannel deferred?
6. **OQ-07** — Speech-tag handling: pass-through only, or something richer?
7. **OQ-08** — Capture STT-minute and TTS-character cost into `TurnProvenance`?
