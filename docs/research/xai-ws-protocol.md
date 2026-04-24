# xAI WebSocket Protocol — Verification Spike

**Status:** 🚧 Awaiting capture. Run `xai-spike` against both endpoints and fill in
the tables below from the captures under `xai-ws-protocol-captures/`.

**Purpose:** the xAI public docs enumerate the WS endpoint URLs and the *shape* of
streaming events, but not the precise event-type strings the server emits, nor the
exact binary-frame expectations on the STT side. This spike is the merge gate
before `proxy/src/stt/xai.rs` or `proxy/src/tts/xai.rs` grow their WS clients —
see `../xai-speech-integration-spec.md` §10.1.

## How to capture

```bash
export PARLEY_XAI_API_KEY=xai-...

# STT: 2-second 16 kHz PCM16 sine tone by default, or --audio path.wav
cargo run -p xai-spike -- stt-ws

# TTS
cargo run -p xai-spike -- tts-ws --text "Hello from parley."
```

Each run writes a `<ts>-<mode>.log` under `xai-ws-protocol-captures/`. Check the
log into the repo (bearer token is never written). If a run turns up an event
type this doc doesn't mention, add a row and link the log line.

## STT — `wss://api.x.ai/v1/stt`

| Direction | Frame kind | `type` string | Body shape | Notes |
|---|---|---|---|---|
| C→S | binary | — | TBD (PCM16? Opus? negotiated via query?) | |
| C→S | text | `audio.done` | `{}` | Close signal documented, verified here. |
| S→C | text | TBD | TBD | Interim transcript delta. |
| S→C | text | TBD | TBD | Final transcript delta. |
| S→C | text | TBD | TBD | Error / rate-limit notice? |

**Close behavior:** TBD — clean close (server sends WS Close frame) vs half-close.

**Binary frame format:** TBD. Candidates to test via `--audio-format`:
`pcm_s16le_16000`, `pcm`, `opus`. Note whether the server rejects without a
format hint.

## TTS — `wss://api.x.ai/v1/tts`

Documented shape (spec §5.5). Verify against capture:

| Direction | Frame | `type` | Notes |
|---|---|---|---|
| C→S | text | `text.delta` | `{ "delta": "..." }` — verified |
| C→S | text | `text.done` | `{}` — verified |
| S→C | text | `audio.delta` | `{ "delta": "<base64>" }` — verify padding |
| S→C | text | `audio.done` | `{ "trace_id": "<uuid>" }` — verify |

**Questions to answer from capture:**
1. Is `audio.delta.delta` standard RFC 4648 base64 (padded), URL-safe, or
   unpadded? The WS bridge code must match what the browser's `atob` accepts.
2. Does `audio.done` always appear, or only on graceful text.done? What about
   server-side errors mid-synthesis?

## Acceptance

When both tables are filled with concrete strings and capture-line references,
update spec §5.3 and §5.5 with the verified values and land this file + the
captures on `feat/xai-speech`. Only then may the WS client code land.
