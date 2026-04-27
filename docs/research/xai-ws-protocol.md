# xAI WebSocket Protocol — Verification Spike

**Status:** ✅ Verified 2026-04-24 against live `wss://api.x.ai/v1/{stt,tts}`.
Captures in `xai-ws-protocol-captures/` are authoritative; tables below summarize.

Spec §5.3 / §5.5 have been updated from this file; treat this doc as the
permanent evidence record, not duplicated documentation.

## How to reproduce

```powershell
$env:PARLEY_XAI_API_KEY = "xai-..."
cargo run -p xai-spike -- stt-ws
cargo run -p xai-spike -- tts-ws --text "Hello from parley."
```

Each run writes `<ts>-<mode>.log` to `xai-ws-protocol-captures/`. Bearer token
is never written to disk.

## STT — `wss://api.x.ai/v1/stt`

**Handshake URL:** `wss://api.x.ai/v1/stt?model=grok-stt&language=en`
(the `audio_format` / `sample_rate` query params documented for REST are NOT
required on the WS — raw PCM16 @ 16 kHz was accepted without any format hint).

**Response headers of note:** `x-trace-id` carries a UUID pinned to the session;
log it for correlation with xAI support tickets.

### Client → Server

| Frame | `type` | Body | Notes |
|---|---|---|---|
| binary | — | raw PCM16 LE bytes | No WAV header needed. We chunked to 4 KB frames; server ingests any size. |
| text | `audio.done` | `{}` | Signals end of input audio. |

### Server → Client

| Frame | `type` | Fields | Notes |
|---|---|---|---|
| text | `transcript.created` | `{id: uuid}` | Session ack emitted immediately after upgrade. Log the id. |
| text | `transcript.partial` | `{text, words: [...], is_final: bool, speech_final: bool, start, duration}` | **Named "partial" but also carries finals** — `is_final: true` marks utterance boundary (segment locked), `speech_final: true` marks end-of-speech. Our `TranscriptEvent::{Partial,Final}` mapping: `is_final=false` → Partial, `is_final=true` → Final. |
| text | `transcript.done` | `{text, words: [], duration}` | Terminal event; `duration` is billable seconds. |

**Close behavior:** xAI does **not** send a WS Close frame. After
`transcript.done` the server drops the TCP connection — `tokio-tungstenite`
surfaces this as `WebSocket protocol error: Connection reset without closing
handshake`. The proxy-side client must treat `transcript.done` (not
connection close) as the stream-terminated signal and swallow the subsequent
reset error without propagating it.

**Word-level shape:** `words` is `[{text, start, end, confidence, speaker}]`
(matches REST response). When `diarize=false` or the segment is empty, the
array is empty.

## TTS — `wss://api.x.ai/v1/tts`

**Handshake URL:** `wss://api.x.ai/v1/tts?language=<bcp47>&voice=<id>&codec=<mp3|...>&sample_rate=<hz>&bit_rate=<bps>`

### Client → Server

| Frame | `type` | Body | Notes |
|---|---|---|---|
| text | `text.delta` | `{delta: string}` | Incremental text. Repeat. |
| text | `text.done` | `{}` | End of input text. |

### Server → Client

| Frame | `type` | Fields | Notes |
|---|---|---|---|
| text | `audio.delta` | `{delta: base64}` | Standard RFC 4648 base64, **padded** with `=`. Decode as-is. |
| text | `audio.done` | `{trace_id: uuid}` | Terminal. |

**Close behavior:** server sends `audio.done` then remains open; client should
close the WS explicitly after receiving it.

## Open items

- `speech_final` semantics: verified as a distinct boolean from `is_final`, but
  xAI docs don't describe when the server decides to emit it. For v1 the
  orchestrator uses `is_final` to mark utterance boundaries and ignores
  `speech_final`.
- STT error events: not exercised in the spike (happy path only). If xAI emits
  a `transcript.error` or similar, that event type is discovered the first
  time production hits it — TODO add a fuzzy-match fallback in the parser.
