# AssemblyAI v3 Streaming — Turn Data Reference

> Model: **u3-rt-pro** (Universal-3 Pro Streaming)
> Protocol: WebSocket (`wss://streaming.assemblyai.com/v3/ws`)
> Source: [API Reference](https://www.assemblyai.com/docs/api-reference/streaming-api/universal-3-pro-streaming/universal-3-pro-streaming), [Message Sequence](https://www.assemblyai.com/docs/streaming/universal-streaming/message-sequence), [u3-rt-pro Guide](https://www.assemblyai.com/docs/streaming/universal-3-pro)

---

## 1. Message Types (Server → Client)

The server sends four message types over the WebSocket:

| Type | When | Purpose |
|---|---|---|
| `Begin` | Once, at session start | Session ID + expiration |
| `SpeechStarted` | When speech first detected | Notification only (u3-rt-pro specific) |
| `Turn` | Repeatedly during speech | Transcript + word-level data |
| `Termination` | Once, at session end | Duration stats + session teardown |

---

## 2. Begin Message

Sent once when the WebSocket handshake completes and the session is ready.

```json
{
  "type": "Begin",
  "id": "de5d9927-73a6-4be8-b52d-b4c07be37e6b",
  "expires_at": 1759796682
}
```

| Field | Type | Description |
|---|---|---|
| `type` | `"Begin"` | Message type discriminator |
| `id` | `string` | UUID session identifier |
| `expires_at` | `integer` | Unix timestamp when the session token expires |

---

## 3. SpeechStarted Message

Sent when speech is first detected in the audio stream. Only available for u3-rt-pro (not Universal-Streaming).

```json
{
  "type": "SpeechStarted",
  ...
}
```

_(Exact fields TBD — the API reference lists 3 properties but doesn't expand them in the public docs. We don't currently use this event in Parley.)_

---

## 4. Turn Message

The primary event. Sent repeatedly during speech — one message per incremental transcript update. This is where all the transcription data lives.

### 4.1 Full Turn JSON example

```json
{
  "type": "Turn",
  "turn_order": 0,
  "turn_is_formatted": true,
  "end_of_turn": false,
  "end_of_turn_confidence": 0.000414,
  "transcript": "Hi, my name is",
  "utterance": "",
  "words": [
    {
      "start": 2160,
      "end": 2560,
      "text": "Hi,",
      "confidence": 0.868781,
      "word_is_final": true
    },
    {
      "start": 2560,
      "end": 2560,
      "text": "my",
      "confidence": 0.987536,
      "word_is_final": true
    },
    {
      "start": 2640,
      "end": 2720,
      "text": "name",
      "confidence": 0.999994,
      "word_is_final": true
    },
    {
      "start": 2720,
      "end": 2720,
      "text": "is",
      "confidence": 0.730668,
      "word_is_final": true
    },
    {
      "start": 3280,
      "end": 3360,
      "text": "Son",
      "confidence": 0.799996,
      "word_is_final": false
    }
  ],
  "speaker_label": null,
  "language_code": null,
  "language_confidence": null
}
```

### 4.2 Turn-level fields (11 properties)

| Field | Type | Always present | Description |
|---|---|---|---|
| `type` | `"Turn"` | ✓ | Message type discriminator |
| `turn_order` | `integer` | ✓ | Incrementing turn counter (0, 1, 2, …). Resets to 0 at session start. Same `turn_order` across all partials of the same turn. |
| `turn_is_formatted` | `boolean` | ✓ | Whether the transcript includes punctuation/capitalization. Always `true` for u3-rt-pro (formatting is built into the model). For u3-rt-pro, `turn_is_formatted` and `end_of_turn` always have the same value. |
| `end_of_turn` | `boolean` | ✓ | `true` = this is the final message for this turn. `false` = partial (more messages coming for this turn). |
| `end_of_turn_confidence` | `float` | ✓ | Model's confidence that the turn has ended (0.0–1.0). For u3-rt-pro, the actual end-of-turn decision is punctuation-based, not confidence-threshold-based. This value is still provided but is informational. |
| `transcript` | `string` | ✓ | Running transcript for the current turn. Accumulates finalized words progressively. May be empty on the very first message of a turn (before any words finalize). **Use this for display.** |
| `utterance` | `string` | ✓ | Populated when an utterance boundary is detected (a pause in speech). Contains the finalized utterance text at that moment. Empty string `""` on all other messages. Useful for eager/pre-emptive LLM inference. Note: the `utterance` field may be populated on a message where `end_of_turn` is still `false` — the utterance ended but the turn hasn't. |
| `words` | `array` | ✓ | Word-level data. See §4.3 below. |
| `speaker_label` | `string?` | Only if `speaker_labels=true` | Speaker identifier (e.g., `"speaker_0"`, `"speaker_1"`). Only present when speaker diarization is enabled in the connection parameters. |
| `language_code` | `string?` | Only if `language_detection=true` | Detected language code (e.g., `"en"`, `"es"`, `"de"`). Only present when language detection is enabled. |
| `language_confidence` | `float?` | Only if `language_detection=true` | Language detection confidence (0.0–1.0). |

### 4.3 Word-level fields (5 properties per word)

Each entry in the `words` array:

| Field | Type | Description |
|---|---|---|
| `start` | `integer` | Start time in milliseconds from session start |
| `end` | `integer` | End time in milliseconds from session start |
| `text` | `string` | The word text. May include trailing punctuation attached by the model (e.g., `"Hi,"`, `"Sonny."`, `"agent."`). |
| `confidence` | `float` | Recognition confidence (0.0–1.0). Lower values indicate less certainty. |
| `word_is_final` | `boolean` | `true` = this word is finalized and won't change. `false` = this word may still change in subsequent Turn messages (e.g., `"Son"` → `"Sonny"` → `"Sonny."`). |

---

## 5. Termination Message

Sent once when the session ends (either by client `Terminate` message, inactivity timeout, or token expiration).

```json
{
  "type": "Termination",
  "audio_duration_seconds": 7,
  "session_duration_seconds": 7
}
```

| Field | Type | Description |
|---|---|---|
| `type` | `"Termination"` | Message type discriminator |
| `audio_duration_seconds` | `integer` | Total audio duration processed (seconds) |
| `session_duration_seconds` | `integer` | Total session wall-clock time (seconds) |

After receiving `Termination`, no further messages will be sent and the WebSocket connection will be closed.

---

## 6. Message Types (Client → Server)

| Message | Purpose | Example |
|---|---|---|
| **Audio data** | Raw binary PCM16 LE audio frames (50ms–1000ms each) | _(binary payload)_ |
| `ForceEndpoint` | Force the current turn to end immediately | `{"type": "ForceEndpoint"}` |
| `Terminate` | Gracefully end the session | `{"type": "Terminate"}` |
| `UpdateConfiguration` | Update config mid-stream | See §6.1 |

### 6.1 UpdateConfiguration

Allows changing certain parameters without reconnecting:

```json
{
  "type": "UpdateConfiguration",
  "keyterms_prompt": ["account number", "routing number"],
  "prompt": "Transcribe verbatim...",
  "max_turn_silence": 5000,
  "min_turn_silence": 200
}
```

Updatable fields: `keyterms_prompt`, `prompt`, `max_turn_silence`, `min_turn_silence`. All optional — include only the fields you want to change.

---

## 7. Connection Parameters (Query String)

Parameters passed in the WebSocket URL query string at connection time:

| Parameter | Type | Default | Description |
|---|---|---|---|
| `speech_model` | `enum` | _(required)_ | `"u3-rt-pro"` for Universal-3 Pro |
| `sample_rate` | `integer` | `16000` | Audio sample rate in Hz |
| `encoding` | `enum` | `pcm_s16le` | Audio encoding: `pcm_s16le` or `pcm_mulaw` |
| `speaker_labels` | `boolean` | `false` | Enable speaker diarization. Adds `speaker_label` to Turn events. |
| `max_speakers` | `integer` | — | Max speakers (1–10). Only with `speaker_labels=true`. |
| `language_detection` | `boolean` | `false` | Return `language_code` + `language_confidence` on Turn events. Note: u3-rt-pro ignores `language_code` as a connection param — use `prompt` to guide language. |
| `min_turn_silence` | `integer` | `100` | Silence (ms) before speculative end-of-turn check. Lower = faster partials but may split entities. |
| `max_turn_silence` | `integer` | `1000` | Max silence (ms) before forcing turn end regardless of punctuation. |
| `vad_threshold` | `float` | `0.3` | VAD confidence threshold (0.0–1.0). Frames below this are considered silent. Increase for noisy environments. |
| `inactivity_timeout` | `integer` | _(none)_ | Seconds of silence before session auto-terminates (5–3600). |
| `keyterms_prompt` | `string[]` | — | Words/phrases to boost recognition accuracy. |
| `prompt` | `string` | _(default prompt)_ | Custom transcription instructions (beta). u3-rt-pro has a built-in default prompt optimized for turn detection. |
| `token` | `string` | — | Temporary auth token (alternative to `Authorization` header). |
| `domain` | `enum` | — | `"medical-v1"` for medical terminology mode. |

---

## 8. Turn Detection (u3-rt-pro specifics)

u3-rt-pro uses a **punctuation-based** turn detection system, not a confidence-threshold system.

### How it works

1. Speech stops → silence begins.
2. When silence reaches `min_turn_silence` (default 100ms), the model transcribes buffered audio and checks for terminal punctuation (`. ? !`).
3. **Terminal punctuation found** → turn ends (`end_of_turn: true`).
4. **No terminal punctuation** → partial emitted (`end_of_turn: false`), turn continues.
5. If silence continues to `max_turn_silence` (default 1000ms) → turn is forced to end regardless of punctuation.

### Key implications

- `end_of_turn_confidence_threshold` has **no impact** on u3-rt-pro (it's for Universal-Streaming only).
- `end_of_turn` and `turn_is_formatted` always have the **same value** for u3-rt-pro.
- **At most one partial** per silence period. Partials are not emitted for every sub-word — only when `min_turn_silence` elapses without terminal punctuation.
- The model applies punctuation intelligently based on vocal tone: `"Pizza."` (statement), `"Pizza?"` (question), `"Pizza---"` (trailing off).

### Recommended configurations

| Use case | `min_turn_silence` | `max_turn_silence` |
|---|---|---|
| Voice agents (fastest) | 100 | 1000 |
| Notetaking / captions | 100–400 | 1000–3000 |
| Long-form dictation (entity-heavy) | 200–400 | 3000–5000 |

⚠️ Setting `min_turn_silence` too low can split entities like phone numbers and emails across turns.

---

## 9. `transcript` vs `utterance` — When to use which

| Field | Updates on | Best for |
|---|---|---|
| `transcript` | Every Turn message | Display / captions / final turn text. Accumulates words progressively. |
| `utterance` | Only at utterance boundaries | Eager LLM inference. Available before `end_of_turn` in many cases. |

### Timing relationship

```
Turn msg 1:  transcript="Hi,"       utterance=""        end_of_turn=false
Turn msg 2:  transcript="Hi, my"    utterance=""        end_of_turn=false
...
Turn msg N:  transcript="Hi, my name is Sonny."
             utterance="Hi, my name is Sonny."          end_of_turn=true
```

When utterance end and turn end coincide, both fields have the same value. When utterance ends before the turn:

```
Turn msg K:  transcript="I am a voice"
             utterance="I am a voice agent."            end_of_turn=false  ← utterance ready!
...
Turn msg M:  transcript="I am a voice agent."
             utterance=""                               end_of_turn=true   ← turn finalized
```

The `utterance` field is populated **exactly once** per utterance boundary. On the `end_of_turn: true` message that follows, `utterance` is empty — use `transcript` to get the complete turn text.

---

## 10. Word Evolution (Partial → Final)

Non-final words (`word_is_final: false`) may change in subsequent Turn messages:

```
Message 1: words = [..., { text: "Son",   confidence: 0.80, word_is_final: false }]
Message 2: words = [..., { text: "Sonny", confidence: 0.90, word_is_final: false }]
Message 3: words = [..., { text: "Sonny", confidence: 0.90, word_is_final: true  }]  (partial)
Message 4: words = [..., { text: "Sonny.", confidence: 0.66, word_is_final: true  }]  (end_of_turn)
```

**Observations:**
- Text can change: `"Son"` → `"Sonny"` (more audio reveals the full word)
- Confidence can change as more context arrives
- Punctuation is **attached to the word text** by the model: `"Sonny"` → `"Sonny."`
- `word_is_final` transitions from `false` → `true` as the model becomes certain
- The last non-final word in the array is the one currently being spoken
- On the `end_of_turn: true` message, all words have `word_is_final: true`

### What the `transcript` field shows vs what `words` contains

The `transcript` field only contains text from **finalized** words. A non-final word at the end of the `words` array is NOT included in `transcript` yet.

```
words:      ["Hi," (final), "my" (final), "name" (final), "is" (final), "Son" (NOT final)]
transcript: "Hi, my name is"     ← "Son" is not in transcript yet
```

This means the `words` array always has **more or equal** content than `transcript`.

---

## 11. Speaker Labels

When `speaker_labels=true` in connection params:

- Each Turn event includes a `speaker_label` field: `"speaker_0"`, `"speaker_1"`, etc.
- The label identifies who is speaking in that turn.
- Use `max_speakers` (1–10) to hint at expected speaker count for better accuracy.
- Speaker labels are consistent within a session but may not persist across sessions.

---

## 12. What Parley Currently Uses

As of the current codebase (`src/stt/assemblyai.rs`), we extract from Turn events:

| Field | Extracted | Used for |
|---|---|---|
| `type` | ✓ | Routing to Turn/Begin/Termination handlers |
| `transcript` | ✓ | Flat string passed to `on_transcript` callback |
| `turn_is_formatted` | ✓ | Passed as `is_formatted: bool` |
| `turn_order` | ✓ | Passed as `turn_order: u32` |
| `end_of_turn` | ✗ | **Not yet extracted** — needed for graph |
| `end_of_turn_confidence` | ✗ | Not extracted |
| `words` | ✗ | **Not yet extracted** — needed for graph |
| `utterance` | ✗ | Not extracted (possible future use for eager formatting) |
| `speaker_label` | ✗ | Not extracted (`speaker_labels` not enabled yet) |
| `language_code` | ✗ | Not extracted |
| `language_confidence` | ✗ | Not extracted |

### What needs to change for the word graph

The `on_transcript` callback signature must be extended to pass:
1. `words: Vec<SttWord>` — word-level data for graph ingestion
2. `end_of_turn: bool` — to control FLAG_TURN_LOCKED clearing
3. `speaker_label: Option<String>` — when multi-speaker is enabled

The flat `transcript` string may still be useful as a fallback or for debugging, but the graph will be built from the `words` array.

---

## 13. Interesting Properties for Future Use

| Field / Feature | Potential Parley use |
|---|---|
| `utterance` | Trigger eager LLM formatting before turn ends (lower latency) |
| `end_of_turn_confidence` | Show a "turn ending..." indicator in UI when confidence > 0.3 |
| `language_code` | Display detected language per turn, auto-switch formatting rules |
| `UpdateConfiguration` | Dynamically boost keyterms based on conversation context |
| `ForceEndpoint` | Already implemented — "Force turn end" button |
| `vad_threshold` | Expose as a setting for noisy environments |
| `prompt` | Custom transcription instructions (e.g., "Transcribe verbatim") |
| `domain: medical-v1` | Medical terminology mode for healthcare use cases |
| `inactivity_timeout` | Auto-stop recording after silence |
