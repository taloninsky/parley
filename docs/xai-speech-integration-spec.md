# xAI Speech Integration — Specification

> Status: **Ready to build** (all §4 Open Questions resolved — see [§14](#14-resolved-decisions))
> Author: Gavin + BigDog
> Date: 2026-04-24

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
- **xAI Voice Agent API** (`wss://api.x.ai/v1/realtime` — unified STT+LLM+TTS socket). Out of scope per [§14 OQ-03 resolution](#14-resolved-decisions); parley retains free choice of LLM across STT/LLM/TTS stages.
- **Deprecating AssemblyAI or ElevenLabs.** Both remain fully supported per [§14 OQ-04 resolution](#14-resolved-decisions). xAI is the fresh-install default ([§14 OQ-01](#14-resolved-decisions)); AssemblyAI and ElevenLabs remain user-selectable and serve as the fallback when xAI has no credential ([§14.1](#141-derived-from-the-above)).
- **Cost aggregation at the session level.** Per-call cost *capture* for STT minutes and TTS characters is in scope (see [§7](#7-cost-accounting)); roll-up display mirrors the deferred LLM cost-aggregation note at [architecture.md line 719](architecture.md).
- **Real-time beamforming / spatial diarization** of xAI STT output. xAI returns word-level `speaker` indexes under `diarize=true`; the spatial coordinate system ([architecture.md §"Spatial Coordinate System"](architecture.md)) is upstream of STT and unaffected.
- **Migration of existing recordings.** Session provenance already pins the provider per session ([architecture.md §"Provenance"](architecture.md)); older sessions keep their AssemblyAI / ElevenLabs artifacts.

---

## 4. Open Questions (Blocking)

These must be resolved before implementation begins. Each carries BigDog's recommendation with rationale; Gavin's explicit decision is required.

### OQ-01 — Default provider selection

> **RESOLVED → (b) xAI is default for both STT and TTS.** See [§14](#14-resolved-decisions) and [§14.1](#141-derived-from-the-above) for the fresh-install fallback chain (xAI → AssemblyAI / ElevenLabs when xAI has no credential).

**Question:** On fresh install / unconfigured state, which STT and TTS provider should the default profile point to?

**Options considered:**
- **(a)** Keep AssemblyAI (STT) and ElevenLabs (TTS) as defaults. xAI is opt-in.
- **(b)** Flip defaults to xAI. AssemblyAI / ElevenLabs remain available.
- **(c)** No default — installer forces the user to pick after seeing a comparison card.

### OQ-02 — STT streaming connection topology

> **RESOLVED → (a) Proxy-through.** Browser → proxy → xAI. The xAI bearer token never leaves the proxy.

**Question:** When the browser captures audio for xAI streaming STT, does the WebSocket go **(a) browser → proxy → xAI** or **(b) browser → xAI directly** (with a short-lived token fetched from the proxy, the way AssemblyAI works today)?

**Options considered:**
- **(a) Proxy-through:** Simpler security posture — the xAI bearer token never leaves the proxy. Adds a hop of latency (~5–15 ms on localhost). Adds a new axum WebSocket route that shuttles binary frames bidirectionally. Matches how ElevenLabs TTS is already proxied.
- **(b) Direct with token exchange:** Lower latency, architecturally consistent with AssemblyAI. Requires xAI to offer a short-lived/scoped token endpoint — **this is not documented** in `docs.x.ai` as of 2026-04-24 and must be verified with xAI support before committing. If xAI bearer tokens are long-lived API keys with no temp-token flow, (b) is off the table because exposing the raw key to the browser violates the invariant in [§2](#2-goals) and [secrets-storage-spec.md](secrets-storage-spec.md).

### OQ-03 — Voice Agent API exposure

> **RESOLVED → (a) Voice Agent API out of scope.** Gavin's rationale: parley intentionally mixes different LLMs across STT → LLM → TTS; a unified vendor socket collapses that freedom.

**Question:** Should parley's architecture explicitly accommodate xAI's unified Voice Agent API (`wss://api.x.ai/v1/realtime`), which collapses STT + LLM + TTS into one socket?

**Options considered:**
- **(a)** Ignore the Voice Agent API. Treat xAI STT and TTS as three independent providers the pipeline stitches together.
- **(b)** Add a new pipeline mode / orchestrator variant that delegates the whole turn to a unified provider, bypassing parley's STT→LLM→TTS stages when that provider is selected.

### OQ-04 — Coexistence trajectory

> **RESOLVED → (a) Permanent coexistence.** Users may mix providers across profiles indefinitely. Reinforces [Philosophy §6](philosophy.md).

**Question:** Is xAI intended to coexist with AssemblyAI / ElevenLabs permanently, or is the long-term plan consolidation to one STT and one TTS vendor?

**Options considered:**
- **(a)** Permanent coexistence. Users may mix providers across profiles.
- **(b)** xAI replaces incumbents after a 30/60/90-day evaluation, via a deprecation path.
- **(c)** Decision deferred; no consolidation plan documented.

### OQ-06 — Diarization and multichannel surface area

> **RESOLVED → (b) Diarization on by default, multichannel deferred.** Per-word speaker indexes project into the word-graph `speaker_lane`. Multichannel waits on the capture pipeline (§12.4).

**Question:** xAI STT supports diarization (`diarize=true`) and multichannel (2–8 channels). Parley's philosophy §3 ("Audio deserves respect") strongly favors preserving channel information. How much of this do we expose in v1?

**Options considered:**
- **(a) MVP surface.** Send mono-mix like today's AssemblyAI path. Ignore `diarize` and `multichannel` request fields. STT returns a single speaker lane.
- **(b) Diarization on, multichannel off.** Enable `diarize=true` by default; let xAI label `speaker` indexes; project them into the word graph's `speaker_lane` field. Still send mono-mix.
- **(c) Full.** Enable both `diarize` and `multichannel` when the capture device supports it. Channels flow through unmixed; xAI runs per-channel STT.

### OQ-07 — Speech-tag affordance

> **RESOLVED → (b) Pass-through only.** Persona prompts may instruct the LLM to use tags; no transformation in this spec. Tag semantics are owned by [expressive-annotation-spec.md](expressive-annotation-spec.md).

**Question:** xAI TTS supports inline speech tags (`[laugh]`, `[sigh]`, `[pause]`) and wrapping tags (`<whisper>`, `<slow>`, `<singing>`). How does the assistant's generated text feed these in? Three places they might come from: (1) the LLM generates them directly, (2) the persona system-prompt instructs the LLM to use them, (3) a dedicated post-LLM formatter inserts them. Does v1 support any of these?

**Options considered:**
- **(a)** No tag support. Plain text to TTS. Tags in assistant output are passed through verbatim (xAI renders them; ElevenLabs would spell them).
- **(b)** Pass-through only. Persona prompts may instruct the LLM to use tags; we don't transform the text. The [expressive-annotation-spec](expressive-annotation-spec.md) already exists — reference it, don't re-specify.
- **(c)** Tag-aware voice-composer. We add a post-LLM stage that parses tags and emits them as `TtsRequest.speech_tags` structured fields. Out of scope for v1.

### OQ-08 — Per-call cost capture

> **RESOLVED → Yes, minimal.** `TurnProvenance` gains `stt_cost: Option<Cost>` and `tts_cost: Option<Cost>` (`Cost` struct from `parley_core::chat`, matching existing LLM/TTS pattern). No aggregation / display UI in this spec.

**Question:** The orchestrator already captures LLM cost per turn in `TurnProvenance` ([architecture.md Conversation orchestrator §"Cost"](architecture.md)). Do we capture STT-minute and TTS-character cost per turn too?

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
wss://api.x.ai/v1/stt?model=grok-stt&language=en
```

Verified 2026-04-24 against the live service — see [`research/xai-ws-protocol.md`](research/xai-ws-protocol.md) and the captures under `research/xai-ws-protocol-captures/` for the source-of-truth evidence.

**Handshake:** `Authorization: Bearer <XAI_API_KEY>` header; `model` and `language` as query params. No `audio_format` or `sample_rate` query param is required — xAI accepts raw PCM16 LE at 16 kHz without a format hint. `x-trace-id` arrives on the upgrade response and should be logged for support correlation.

**Client → Server:**
- Binary frames carrying raw PCM16 LE bytes (any chunk size; we send 4 KB).
- Single text frame `{"type":"audio.done"}` to signal end of input.

**Server → Client:**
- `{"type":"transcript.created","id":"<uuid>"}` — session ack, emitted once immediately after the upgrade.
- `{"type":"transcript.partial","text":"…","words":[…],"is_final":bool,"speech_final":bool,"start":f,"duration":f}` — interim *and* locked-segment events share this type. Treat `is_final:false` as `TranscriptEvent::Partial`, `is_final:true` as `TranscriptEvent::Final`. `speech_final` is advisory only and is ignored in v1. `words` has the same shape as the REST response (`{text,start,end,confidence,speaker}`), and is empty for empty segments.
- `{"type":"transcript.done","text":"…","words":[],"duration":f}` — terminal event; `duration` is billable seconds.

**Close behavior:** xAI **does not send a WS Close frame**. After `transcript.done` the server drops the TCP connection; `tokio-tungstenite` surfaces this as `Connection reset without closing handshake`. The WS client must treat `transcript.done` as the stream terminator and swallow the subsequent reset without propagating it as an error.

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

Verified 2026-04-24 against the live service — see [`research/xai-ws-protocol.md`](research/xai-ws-protocol.md).

- Client → Server: `{"type":"text.delta","delta":"…"}` (repeated), then `{"type":"text.done"}`.
- Server → Client: `{"type":"audio.delta","delta":"<base64>"}` (repeated), then `{"type":"audio.done","trace_id":"<uuid>"}`.
- `audio.delta.delta` is **standard RFC 4648 base64, padded with `=`** — decode as-is with `base64::engine::general_purpose::STANDARD` (no URL-safe alphabet, no padding fixup).
- `audio.done` is terminal; the client closes the WS after receiving it (unlike STT, TTS does not drop the TCP connection itself).

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
use parley_core::chat::Cost;

#[async_trait]
pub trait SttProvider: Send + Sync {
    fn id(&self) -> ProviderId;

    /// File / batch transcription.
    async fn transcribe(&self, req: SttRequest) -> Result<Transcript, SttError>;

    /// Streaming transcription. Returns a pair of channels:
    ///   tx_audio: sink for PCM16 LE binary frames
    ///   rx_events: stream of TranscriptEvent (interim/final)
    async fn stream(&self, config: SttStreamConfig) -> Result<SttStreamHandle, SttError>;

    /// Cost for `seconds` of input audio at this provider's current rates.
    /// Returns the same `Cost` struct used by `TtsProvider::cost` and the
    /// LLM providers, so `TurnProvenance` can carry all three uniformly.
    fn cost(&self, seconds: f64, streaming: bool) -> Cost;
}
```

The `Cost` return type is deliberate: matches `TtsProvider::cost(u32) -> Cost` at [`proxy/src/tts/mod.rs`](../proxy/src/tts/mod.rs) and the LLM provider pattern, so `TurnProvenance.stt_cost`, `tts_cost`, and `llm_cost` share one serde shape.

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

The `SttProvider` trait defined in §6.2 is a **proxy-side** trait. Frontend providers are not required to implement it. Routing authority across both topologies lives in the orchestrator — see [§6.6](#66-orchestrator-owns-routing) for the routing-ownership model.

### 6.6 Orchestrator Owns Routing

Per Gavin's direction, the orchestrator — not the frontend, not the provider — is the single authority that decides *and executes* the STT → LLM → TTS pathway for each turn. This is the shape the existing TTS slice already follows (`OrchestratorContext.tts` drives the `Speaking` state, [`proxy/src/orchestrator/mod.rs`](../proxy/src/orchestrator/mod.rs)); xAI STT extends the same pattern to capture.

**Decision surface owned by the orchestrator:**
- Which STT provider runs for a given capture session (reading the active profile's `stt.provider` field).
- Which LLM provider runs for the assistant turn (already owned).
- Which TTS provider and voice run for assistant speech (already owned).
- Fallback selection per [§14.1](#141-derived-from-the-above) when the preferred provider's credential is unresolved.

**Execution surface:**
- *Proxy-through providers* (xAI STT, xAI TTS, ElevenLabs TTS): the orchestrator holds the `Arc<dyn SttProvider>` / `Arc<dyn TtsProvider>` and drives the upstream call directly.
- *Frontend-direct providers* (AssemblyAI STT today): the orchestrator emits an `OrchestratorCommand::StartSttDirect { provider: ProviderId::AssemblyAi, session_token: String, config: SttStreamConfig }` frame on the conversation SSE. The frontend treats this as a peripheral instruction — it opens the direct WS to AssemblyAI, relays transcript events back to the proxy via `POST /api/conversation/{id}/transcript` as they arrive, and closes on `StopStt`. The frontend is the *I/O hand* of the orchestrator, not an independent actor.

Either topology produces the same `TranscriptEvent` flow into the orchestrator; downstream LLM + TTS stages are topology-agnostic. This preserves the "core owns the pipeline" invariant from [Philosophy §6](philosophy.md) while accommodating AssemblyAI's documented temp-token flow without pushing decision-making into the browser.

**Files added / changed:**
- `proxy/src/orchestrator/stt_router.rs` (new) — pure function `select_stt_provider(profile, registry, secrets) -> SttSelection` that encodes the fallback chain ([§14.1](#141-derived-from-the-above)). Unit-testable without I/O.
- `proxy/src/orchestrator/mod.rs` — `OrchestratorContext` gains `stt: HashMap<ProviderId, Arc<dyn SttProvider>>` and `stt_router: StttRouter`. The orchestrator's dispatch loop dispatches capture-start to the selected provider (proxy-side) or emits `StartSttDirect` (frontend-side).
- `src/conversation/capture.rs` (frontend) — consumes `StartSttDirect` / `StopStt` commands; no provider-selection logic in the frontend.

---

## 7. Cost Accounting

`TurnProvenance` gains:

```rust
use parley_core::chat::Cost;

pub struct TurnProvenance {
    // existing fields (including llm_cost: Option<Cost>)…
    pub stt_cost: Option<Cost>,
    pub tts_cost: Option<Cost>,
}
```

Populated at turn append time by the orchestrator:
- `stt_cost = Some(stt_provider.cost(duration_sec, streaming))` when STT ran for the turn.
- `tts_cost = Some(tts_provider.cost(char_count))` when TTS ran for the turn.

All three fields use the same `Cost` struct from `parley_core::chat`, so provenance has one uniform cost shape across STT / LLM / TTS regardless of which providers the turn actually used.

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

**Validation.** The profile loader validates `stt.provider` and `tts.provider` against the `REGISTRY` at [`proxy/src/providers.rs`](../proxy/src/providers.rs) — any value not present in the registry's STT (or TTS) category rows is rejected at load time with a `ProfileError::UnknownProvider { category, value }` variant. Unknown values do not silently fall back to the default; the profile is refused until corrected. This mirrors the secrets-spec pattern ([secrets-storage-spec.md §4.3](secrets-storage-spec.md)) where `ProviderId` is the source of truth and the TOML string must round-trip through it.

### 9.2 New Voice Picker Component

`src/ui/voice_picker.rs` (new). Generic across TTS providers. Fetches `/api/tts/voices?provider={id}` on mount; renders a dropdown with the voice `display_name`; stores selection in the active profile signal.

Used in:
- Settings → Profile editor (future — when profile CRUD UI lands).
- Conversation view's active-persona panel (immediately — lets the user swap voice mid-session for the next assistant turn).

#### 9.2.1 STT Provider Picker Component

`src/ui/stt_provider_picker.rs` (new). Sibling of the voice picker. Pure provider dropdown — no voice concept on STT. Renders the list of `ProviderCategory::Stt` providers from `/api/secrets/status`; disables entries whose credential is unconfigured; stores selection in the active profile signal.

Used in the same two surfaces as the voice picker. This component is net-new work forced by OQ-01 flipping the default away from AssemblyAI — without it, a user on the new default has no path to switch back to AssemblyAI except editing the profile TOML by hand.

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

**Acceptance (merge gate):** the spike PR lands `docs/research/xai-ws-protocol.md` *and* updates [§5.3](#53-stt--websocket-streaming) and [§5.5](#55-tts--websocket-streaming) in this spec with the verified event-type strings and any binary-frame corrections. Wire-log captures (redacted of the bearer token) are checked into `docs/research/xai-ws-protocol-captures/` for reproducibility. No §10.2 step beyond (3) / (4) — where WS code is written — may begin until the spike PR is merged. If the spike reveals the docs disagree with reality, the spec updates are the load-bearing output; the `docs/research` markdown is the evidence.

### 10.2 Implementation Task Order (post-spike)

1. `proxy/src/providers.rs` — add `Xai` variant + two registry entries (§6.1).
2. `parley-core/src/stt.rs`, `parley-core/src/tts.rs` — canonical types (§6.2, §6.3). Includes `Cost` used by both `SttProvider::cost` and `TtsProvider::cost`.
3. `proxy/src/stt/mod.rs` + `proxy/src/stt/xai.rs` — REST client first, WS second. Unit-tested against a local `wiremock` HTTP fixture and a small `tokio-tungstenite` echo fixture.
4. `proxy/src/tts/xai.rs` — REST then WS.
5. `proxy/src/stt_api.rs`, `proxy/src/tts_api.rs` — HTTP handlers from §8.
6. `proxy/src/orchestrator/stt_router.rs` + `OrchestratorContext` extension (§6.6) — pure-function provider selection with the §14.1 fallback chain; unit-tested with a synthetic `REGISTRY` and `SecretsManager`.
7. `src/conversation/capture.rs` — handle `StartSttDirect` / `StopStt` commands for the AssemblyAI frontend-direct path (§6.6).
8. `parley-core/src/tts.rs` — extend `TtsProvider::voices()`.
9. `src/ui/voice_picker.rs` — component, wired into conversation view.
10. `src/ui/stt_provider_picker.rs` — component (§9.2.1), wired into conversation view.
11. Profile schema extension (§9.1) including the `ProfileError::UnknownProvider` validation path.
12. Provenance extension (§7) — `stt_cost` and `tts_cost` populated at turn append time.
13. Stale-docs sweep: [`proxy/src/orchestrator/mod.rs:7`](../proxy/src/orchestrator/mod.rs) docstring and [`architecture.md`](architecture.md) deferred-slice language around line 706.

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
- **WS bridge event-name normalization** — the mock xAI backend emits whatever event-type strings the §10.1 spike verified (e.g. `"partial_transcript"`); the bridge must translate these to parley's canonical `"transcript.delta"` / `"transcript.final"` before they reach the browser. Test sends the full event-type matrix (interim, final, error, session_begins, session_ends) through the bridge and asserts every inbound string is either canonicalized or rejected; no unknown string is passed through verbatim. This is the core logic of the proxy-through topology; if it regresses, the frontend sees raw vendor event names and breaks.
- `POST /api/tts/synthesize` returns `audio/mpeg` body matching the mock's bytes verbatim.
- `GET /api/tts/voices?provider=xai` caches response (second call does not hit upstream).
- Upstream 5xx → proxy returns `502 upstream_error` and does not leak upstream response body.
- Upstream 429 → `Retry-After` preserved.
- WS mid-stream disconnect → error frame + clean close.
- **Orchestrator STT routing** — given a profile with `stt.provider = "xai"` and no xAI credential configured but AssemblyAI credential present, `select_stt_provider` returns AssemblyAI, and a `ProviderFallback` log line is emitted ([§14.1](#141-derived-from-the-above)). Reverse case — only xAI configured — returns xAI. Neither configured — returns `ProviderUnconfigured`.

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
- `proxy/src/orchestrator/stt_router.rs` — provider-selection with fallback chain (§6.6, §14.1).
- `OrchestratorContext.stt` extension + `StartSttDirect` / `StopStt` command emission for frontend-direct providers (§6.6).
- `src/conversation/capture.rs` — consume `StartSttDirect` commands and relay AssemblyAI direct-connect transcripts.
- `TtsProvider::voices()` method on the existing trait; xAI implements, ElevenLabs stubs to empty.
- `src/ui/voice_picker.rs` — component.
- `src/ui/stt_provider_picker.rs` — component (new, per §14.1).
- Profile schema extension (§9.1), including `ProfileError::UnknownProvider` validation against `REGISTRY`.
- Profile-default change: fresh-install profiles point at xAI for both STT and TTS.
- `TurnProvenance.stt_cost` / `tts_cost` fields (§7), both typed `Option<Cost>`.
- The verification spike (§10.1) — docs/research/xai-ws-protocol.md — with §5.3 / §5.5 updated to verified reality as the merge gate.
- Unit tests (§11.1), HTTP-layer tests (§11.2), manual verification checklist (§11.3).
- `docs/architecture.md` update: new proxy modules, new HTTP routes, provider categorization notes.
- **Stale-docs fix**: the "later slice" docstring at [`proxy/src/orchestrator/mod.rs:7`](../proxy/src/orchestrator/mod.rs) and the deferred-TTS-slice callouts in [`architecture.md`](architecture.md) around line 706 are both obsolete (the voice slice has shipped). Correct them as part of landing this spec.

### 13.2 Out of Scope (Deferred / Tracked)

- xAI TTS WebSocket streaming dispatch (§12.1). v1 routes xAI TTS through the existing per-sentence HTTP loop the orchestrator already drives for ElevenLabs.
- Voice Agent API (§12.2).
- STT direct-connect for xAI (§12.3).
- Multichannel capture pipeline (§12.4).
- AssemblyAI / ElevenLabs deprecation — permanent coexistence per [§14 OQ-04](#14-resolved-decisions).
- Profile CRUD UI — voice picker and STT picker ship, but the full profile editor is its own work.
- Session-level cost aggregation.

---

## 14. Resolved Decisions

Captured here as Gavin reviews the spec; each resolution tightens the build target in §4 and §13.

- **OQ-01 → Option (b) — xAI is the default STT and TTS provider on fresh install.** AssemblyAI and ElevenLabs remain available as alternates. Downstream effects: §9.1 profile example is already consistent; the Settings / profile-creation flow must surface xAI's STT and TTS picker pre-populated with `provider = "xai"`.
- **OQ-02 → Option (a) — proxy-through for xAI STT streaming.** Browser → proxy → xAI WebSocket. AssemblyAI stays on its direct-from-browser token-exchange path. Two-topology STT confirmed; see §6.5.
- **OQ-03 → Option (a) — Voice Agent API (`/v1/realtime`) is explicitly out of scope.** Gavin's rationale: parley intentionally mixes different models across STT / LLM / TTS stages; a unified vendor socket collapses that boundary and removes that freedom. Reinforces [Philosophy §6](philosophy.md).
- **OQ-04 → Option (a) — permanent coexistence.** Users may mix providers across profiles indefinitely; no deprecation roadmap for AssemblyAI or ElevenLabs.
- **OQ-06 → Option (b) — diarization on by default, multichannel deferred.** `diarize=true` is the shipped default for xAI STT; per-word `speaker` indexes are projected into the word-graph `speaker_lane`. Multichannel waits on the capture pipeline (§12.4).
- **OQ-07 → Option (b) — pass-through speech tags.** xAI receives `text` verbatim (tags preserved); transformation / annotation strategy is owned by [expressive-annotation-spec.md](expressive-annotation-spec.md), not this spec.
- **OQ-08 → Yes, minimal.** `TurnProvenance` gains `stt_cost: Option<Cost>` and `tts_cost: Option<Cost>` as specified in §7. No aggregation / display UI in this spec.

### 14.1 Derived from the above

- **STT provider-selection UX does not exist** today (only provider-credential config does). It lands in this spec as a peer of the new TTS voice picker ([§9.2.1](#921-stt-provider-picker-component)). Without it, flipping the default to xAI leaves the user no way to switch back without hand-editing profile TOML.
- The default change (OQ-01) does **not** affect existing configured profiles — they keep their explicit `provider = "..."` values. Only fresh-install / unconfigured profiles pick up the new default.
- **Fresh-install fallback chain.** The orchestrator's `select_stt_provider` / `select_tts_provider` routines ([§6.6](#66-orchestrator-owns-routing)) resolve the active provider at turn-start time:
  1. Read the profile's `stt.provider` / `tts.provider` field. Default on fresh install: `"xai"` for both.
  2. If the selected provider's credential (`SecretsManager::resolve(provider, credential)`) returns `Ok`, use it.
  3. Otherwise, fall back to the **first configured provider** in this category in a fixed preference order:
     - STT: `xai` → `assemblyai`.
     - TTS: `xai` → `elevenlabs`.
  4. If no provider in the category has a configured credential, the orchestrator emits an `OrchestratorEvent::ProviderUnconfigured { category, attempted: Vec<ProviderId> }` event; the UI renders the existing "configure your key" banner. No fabricated audio / transcript.
- The fallback is silent-but-observable: the actual provider used is captured in `TurnProvenance.stt_provider` / `tts_provider` (existing fields), and a `ProviderFallback { requested: xai, used: assemblyai, reason: credential_unresolved }` log line is emitted at turn-start. This means a user who configures AssemblyAI but never xAI gets a working session on fresh install, and can see from provenance exactly which provider answered each turn.
- The fallback chain applies identically to streaming STT and batch STT — both go through `select_stt_provider`.

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
