# Multi-Speaker Capture & Transfer — Specification

> Status: **Draft**
> Author: Gavin + Copilot
> Date: 2026-03-20

---

## Overview

This spec covers five features that together enable Parley to capture, label, and export multi-person conversations (e.g., a Zoom call with a remote collaborator):

1. **System audio capture** via `getDisplayMedia`
2. **Dual-session STT architecture** — one AssemblyAI WebSocket per audio source
3. **Settings UI** — speaker names, audio source assignment, feature toggles
4. **Speaker-labeled transcript** — diarized, optionally timestamped output
5. **Transfer / export button** — combo button for saving or sending transcript to a target

---

## 1. System Audio Capture

### Problem

`BrowserCapture::start()` uses `getUserMedia`, which captures **microphone input only**. Audio playing through speakers (e.g., a remote participant's voice on Zoom) is system output and never reaches `getUserMedia`.

### Solution

Add a second capture mode using the browser's `getDisplayMedia` API with `audio: true`. On Windows Chrome, this presents a "Share system audio" checkbox that captures the system audio mix (everything going to speakers).

### Implementation

#### 1.1 New constructor: `BrowserCapture::start_system_audio`

File: `src/audio/capture.rs`

```rust
pub async fn start_system_audio(
    on_audio: impl Fn(Vec<f32>) + 'static,
) -> Result<Self, JsValue>
```

- Calls `navigator.mediaDevices.getDisplayMedia({ video: true, audio: true })`.
  - `video: true` is required by the spec — browsers won't allow `getDisplayMedia` with video disabled. The video track is immediately stopped after acquisition (we only want audio).
- Creates an `AudioContext` at 16 kHz, connects a `ScriptProcessorNode` exactly like the mic path.
- Returns the same `BrowserCapture` handle. The `stop()` method already stops all tracks, so cleanup is unchanged.

#### 1.2 Browser compatibility note

- **Chrome (Windows):** Full support. The share dialog shows a "Share system audio" checkbox.
- **Chrome (macOS):** Tab audio only, not full system audio. Users on macOS would need a virtual audio cable for full system capture.
- **Firefox:** `getDisplayMedia` audio support is limited. Not a launch blocker — document it.
- **Safari:** No `getDisplayMedia` audio. Not supported.

Parley should detect when the acquired `MediaStream` has no audio tracks and surface an error: *"System audio not available — your browser may not support this feature."*

### Done criteria

- [ ] `BrowserCapture::start_system_audio()` exists and compiles.
- [ ] Calling it opens the browser's screen-share dialog with audio option.
- [ ] Audio from speakers (e.g., a YouTube video) produces PCM samples in the callback.
- [ ] The video track is immediately stopped (no screen recording, no preview).
- [ ] If the user denies permission or the stream has no audio tracks, a clear error is returned.

---

## 2. Dual-Session STT Architecture

### Problem

With two audio sources (mic + system audio), we need two independent STT pipelines so that transcript text is tagged by source, enabling speaker attribution without AI diarization.

### Solution

When multi-speaker mode is active, Parley opens **two** `AssemblyAiSession` WebSocket connections, each receiving audio from one `BrowserCapture` stream. Each session's `on_transcript` callback tags turns with the speaker identity.

### Implementation

#### 2.1 Session management

File: `src/ui/app.rs`

Currently there is one `session_handle` and one `capture_handle`. In multi-speaker mode:

| Handle | Mic (local speaker) | System audio (remote speaker) |
|--------|---------------------|-------------------------------|
| `capture_handle` | `BrowserCapture::start()` | `BrowserCapture::start_system_audio()` |
| `session_handle` | `AssemblyAiSession::connect()` | `AssemblyAiSession::connect()` |

Both sessions use the same API key / temp token (each needs its own token fetch — two tokens total).

#### 2.2 Speaker tagging in callbacks

Each `on_transcript` closure knows which speaker it represents (from settings). When committing a turn to the transcript signal, it prepends the speaker tag:

```
[Gavin] So I was thinking about the architecture…
[Dave] Yeah, that makes sense.
```

The tag format is `[Name] ` — bracket-wrapped name, single space, then the text.

#### 2.3 Single-speaker mode (default)

When only one speaker is configured (the current behavior), everything works exactly as it does today. No speaker tags are prepended. No second session is opened. This is the zero-config default.

#### 2.4 Dual current-turn UI

In multi-speaker mode the single "Current turn" box is replaced by **two side-by-side boxes**, one per speaker. Each box is independent:

```
┌─ Transcript (editable, full width) ─────────────────────────────┐
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
┌─ Gavin speaking… ──────────────┐ ┌─ Dave speaking… ───────────────┐
│ "So I was thinking about…"     │ │ "Yeah that makes sense…"       │
│              [⏎ End Turn]      │ │               [⏎ End Turn]     │
└────────────────────────────────┘ └─────────────────────────────────┘
[ ■ Stop ]  [ Copy ▾ ]  [ Clear ]
```

- Each box shows its speaker's name in the label (e.g., *"Gavin speaking…"*).
- Each box has its own **⏎ End Turn** button that force-commits only that speaker's current turn. The button label is simply "⏎ End Turn" (no name suffix) — the visual association with its box is sufficient.
- The **■ Stop** button in the bottom bar stops **everything** — both captures and both sessions.
- In single-speaker mode, the layout is unchanged: one full-width current-turn box, one End Turn button in the bottom bar (current behavior).

#### 2.5 Full-width layout

The app container currently has `max-width: 720px`. In multi-speaker mode this becomes cramped with two side-by-side boxes. Change: **remove the max-width cap entirely** so the layout stretches to fill the browser window. This benefits both single- and multi-speaker modes — users who want a wider transcript can simply resize their browser.

#### 2.6 Shared state

Both sessions share:
- The same `transcript` signal (they append to it in arrival order)
- The same `countdown_secs` / idle timeout (reset by activity on *either* session)

Each session has its own:
- `partial` signal (one per speaker, displayed in its own current-turn box)
- `current_turn` / `current_turn_order` tracking

#### 2.7 Token budget

Two concurrent sessions = 2× the AssemblyAI usage. This should be noted in the settings UI so users are aware.

### Done criteria

- [ ] In multi-speaker mode, two `BrowserCapture` instances run simultaneously (mic + system audio).
- [ ] Two `AssemblyAiSession` WebSocket connections are opened (one per source).
- [ ] Each session's transcript output is tagged with the configured speaker name.
- [ ] Two side-by-side current-turn boxes are rendered, each with its own partial text and "⏎ End Turn" button.
- [ ] Each End Turn button commits only its speaker's turn.
- [ ] Stop terminates both sessions and both captures.
- [ ] Idle timeout resets on activity from either session.
- [ ] Clear resets state for both sessions.
- [ ] The app layout stretches to full browser width (no max-width cap).
- [ ] In single-speaker mode, behavior is identical to current (no regressions).

---

## 3. Settings UI

### Current state

The settings drawer (gear icon) has:
- AssemblyAI API Key
- Idle timeout (minutes)
- Anthropic API Key (paragraph detection)

### New settings

Add a **Speakers** section below the existing fields. The section contains:

#### 3.1 Speaker cards

Each speaker is a collapsible card with:

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| **Name** | text input | "Me" (first) / "Remote" (second) | Used in `[Name]` transcript tags |
| **Audio source** | dropdown | "Microphone" (first) / "System Audio" (second) | Options: "Microphone", "System Audio" |
| **Enabled** | toggle | ON (first) / OFF (second) | Disabling the second speaker = single-speaker mode |

The first speaker card is always present and cannot be removed. The second speaker card is present but disabled by default. Enabling it activates multi-speaker mode.

#### 3.2 Feature toggles (visible when second speaker is enabled)

| Toggle | Default | Effect |
|--------|---------|--------|
| **Speaker labels** | ON | Prepend `[Name]` tags to committed turns |
| **Timestamps** | OFF | Prepend `[HH:MM:SS]` before each committed turn |

When timestamps are enabled, the format is:

```
[00:00:12] [Gavin] So I was thinking about the architecture…
[00:00:28] [Dave] Yeah, that makes sense.
```

Timestamps are relative to the start of the recording session (not wall-clock time).

When only one speaker is enabled, speaker labels and timestamps toggles are hidden (labels would be redundant with one speaker).

#### 3.2.1 Haiku paragraph detection scaling

The existing Haiku formatting pass sends a window of chunks for paragraph detection. The defaults change based on speaker count:

| Mode | Window size (chunks) | Editable chunks | Context-only chunks |
|------|---------------------|-----------------|--------------------|
| **Single-speaker** | 3 | 2 | 1 |
| **Multi-speaker** | 6 | 4 | 2 |

In multi-speaker mode, each speaker's committed turn is its own paragraph (new line). Even a single word from a different speaker starts a new paragraph. The Haiku prompt is updated to enforce this: *"Each speaker tag (`[Name]`) must begin a new paragraph. Never merge text from different speakers into the same paragraph."*

These defaults can be overridden in settings in the future, but for now they are hardcoded based on whether Speaker 2 is enabled.

#### 3.3 Persistence

All new settings are saved to cookies using the existing `save()`/`load()` pattern:

| Cookie key | Value |
|------------|-------|
| `parley_speaker1_name` | String |
| `parley_speaker2_name` | String |
| `parley_speaker1_source` | `"mic"` or `"system"` |
| `parley_speaker2_source` | `"mic"` or `"system"` |
| `parley_speaker2_enabled` | `"true"` or `"false"` |
| `parley_show_labels` | `"true"` or `"false"` |
| `parley_show_timestamps` | `"true"` or `"false"` |

#### 3.4 Layout

```
┌─ Settings ──────────────────────────────────┐
│                                             │
│  AssemblyAI API Key                         │
│  ┌─────────────────────────────────────┐    │
│  │ ****************************        │    │
│  └─────────────────────────────────────┘    │
│                                             │
│  Idle timeout (minutes)                     │
│  ┌─────────┐                                │
│  │ 5       │                                │
│  └─────────┘                                │
│                                             │
│  Anthropic API Key (paragraph detection)    │
│  ┌─────────────────────────────────────┐    │
│  │ ****************************        │    │
│  └─────────────────────────────────────┘    │
│                                             │
│  ── Speakers ──────────────────────────     │
│                                             │
│  Speaker 1 (You)                            │
│  Name  ┌──────────┐  Source ┌───────────┐   │
│        │ Gavin    │         │Microphone▾│   │
│        └──────────┘         └───────────┘   │
│                                             │
│  Speaker 2 (Remote)              ┌────┐     │
│  Enabled                         │ ○──│     │
│  Name  ┌──────────┐  Source ┌────┴────┴─┐   │
│        │ Dave     │         │Sys Audio▾ │   │
│        └──────────┘         └───────────┘   │
│                                             │
│  ┌─ Options (when Speaker 2 enabled) ──┐    │
│  │  ☑ Speaker labels ([Name] prefix)   │    │
│  │  ☐ Timestamps ([HH:MM:SS] prefix)   │    │
│  └─────────────────────────────────────┘    │
│                                             │
│  ⚠ Two speakers = 2× AssemblyAI usage      │
│                                             │
│  [Close]                                    │
└─────────────────────────────────────────────┘
```

### Done criteria

- [ ] Speaker 1 card is always visible with name input and audio source dropdown.
- [ ] Speaker 2 card has an enable/disable toggle. Disabled by default.
- [ ] Enabling Speaker 2 reveals the speaker labels and timestamps toggles.
- [ ] All new settings persist across page reloads (cookies).
- [ ] Changing speaker names while recording updates future transcript tags immediately.
- [ ] The audio source dropdown offers "Microphone" and "System Audio".
- [ ] A note about 2× API usage is visible when Speaker 2 is enabled.

---

## 4. Speaker-Labeled Transcript

### Behavior

When multi-speaker mode is active and speaker labels are ON:

1. Each committed turn is prefixed with `[SpeakerName] `.
2. If timestamps are also ON, the timestamp comes first: `[HH:MM:SS] [SpeakerName] `.
3. Each speaker's current-turn box (see §2.4) shows their name in the label: *"Gavin speaking…"* / *"Dave speaking…"*.
4. Both speakers can produce partial results simultaneously — each is displayed in its own box.

### Per-speaker paragraph breaks

Every speaker change forces a new paragraph in the transcript, even if the turn is a single word. This means the transcript is always visually grouped by speaker:

```
[Gavin] So I was thinking about the architecture for this feature.

[Dave] Yeah.

[Gavin] And I think we should use a normalized approach.
```

The separator between turns from different speakers is `\n\n` (blank line). Consecutive turns from the **same** speaker are separated by a single space (appended to the existing paragraph), matching current single-speaker behavior.

### Timestamp calculation

- A `session_start_time` is recorded (via `js_sys::Date::now()`) when the Record button is pressed.
- When a turn is committed, the current time minus `session_start_time` gives the offset in milliseconds.
- This is formatted as `[HH:MM:SS]` (hours omitted if zero, so `[05:23]` for 5 min 23 sec, `[1:05:23]` for 1 hour 5 min 23 sec).

### Transcript editing

The transcript remains editable. Users can freely edit speaker tags and timestamps after they're inserted. Parley does not enforce or re-validate them — they're plain text.

### Single-speaker mode

No tags, no timestamps, no changes from current behavior.

### Done criteria

- [ ] In multi-speaker mode with labels ON, each committed turn starts with `[Name] `.
- [ ] In multi-speaker mode with timestamps ON, each committed turn starts with `[MM:SS] ` (or `[H:MM:SS]`).
- [ ] With both ON, format is `[MM:SS] [Name] Text here`.
- [ ] The "Speaking…" indicator shows the active speaker's name.
- [ ] The transcript textarea remains fully editable.
- [ ] Single-speaker mode has zero visible changes from current behavior.

---

## 5. Transfer / Export Button

### Problem

Currently, the only way to get text out of Parley is the Copy button (clipboard). For an AI-assisted workflow, users want to send transcript directly to a target — initially a file, eventually VS Code's chat input.

### Solution

A **combo button** that replaces the current Copy button. It has:
- A **main click area** whose label reflects the currently selected mode.
- A **dropdown arrow** (▾) that opens a menu of available modes plus a settings checkbox.

The button label changes to match the active mode:

| Mode | Button label |
|------|--------------|
| Copy to Clipboard | **Copy** |
| Save as Text | **TXT File** |
| Save as Markdown | **MD File** |
| Send to VS Code | **VS Code** |

### Transfer modes

#### 5.1 Copy to Clipboard (default — current behavior)

No change. This is the existing Copy button, now accessible as a transfer mode. It remains the default.

#### 5.2 Save as Text File

Triggers a browser download of the transcript as a `.txt` file.

- Default filename: `parley-YYYY-MM-DD-HHMMSS.txt`
- Content: the raw transcript text (exactly what's in the textarea).
- Uses the standard `<a download>` trick or the File System Access API if available.
- If **Prompt for filename** is enabled (see §5.5), a dialog is shown before saving.

#### 5.3 Save as Markdown File

Triggers a browser download of the transcript as a `.md` file.

- Default filename: `parley-YYYY-MM-DD-HHMMSS.md`
- Content: YAML frontmatter + transcript body.

```markdown
---
title: Parley Transcript
date: 2026-03-20T14:30:00
speakers:
  - Gavin
  - Dave
duration: "12:45"
---

[00:00:12] [Gavin] So I was thinking about the architecture…
[00:00:28] [Dave] Yeah, that makes sense.
```

Frontmatter is only included when multi-speaker mode is active. In single-speaker mode, the file is just the raw text with a `# Parley Transcript` header and date.

If **Prompt for filename** is enabled (see §5.5), a dialog is shown before saving.

#### 5.4 Send to VS Code (future — not in initial build)

A VS Code extension (`parley-bridge`) opens a local WebSocket server (e.g., `ws://localhost:3034`). Parley connects and sends transcript text. The extension inserts it into the active chat input or editor.

This mode appears in the dropdown as grayed-out with a "Coming soon" label until the extension is built. It is **out of scope** for this implementation round but is documented here for future reference.

#### 5.5 Prompt for filename (checkbox in dropdown)

The dropdown menu includes a checkbox option at the bottom:

```
☐ Prompt for filename
```

When enabled, the TXT File and MD File modes show a `window.prompt()` dialog (or File System Access `showSaveFilePicker` where available) allowing the user to specify the filename before download. When disabled, files are saved immediately with the auto-generated name. This setting persists via cookie (`parley_prompt_filename`, `"true"` / `"false"`, default `"false"`).

This checkbox has no effect on Copy to Clipboard or Send to VS Code modes.

### UI

The combo button replaces the current Copy button when text is present:

```
┌──────────┬───┐
│  Copy    │ ▾ │
└──────────┴───┘
```

The main area label changes based on the selected mode (see table above). Clicking the dropdown arrow shows:

```
┌──────────────────────────────┐
│ ✓ Copy                       │
│   TXT File                   │
│   MD File                    │
│   VS Code (coming soon)      │
│ ─────────────────────────    │
│ ☐ Prompt for filename        │
└──────────────────────────────┘
```

The selected mode persists across sessions (cookie: `parley_transfer_mode`).

A brief confirmation appears after transfer (same pattern as current "✓ Copied" feedback):
- "✓ Copied" for Copy mode
- "✓ Saved" for file downloads (or "✓ Saved as notes.md" if filename was prompted)
- "✓ Sent" for VS Code mode (future)

### Done criteria

- [ ] The combo button renders with a main click area and a dropdown arrow.
- [ ] The button label reflects the active mode: "Copy", "TXT File", "MD File", or "VS Code".
- [ ] Clicking the dropdown shows the mode menu with a separator and the "Prompt for filename" checkbox.
- [ ] "Copy" works identically to the current Copy button.
- [ ] "TXT File" triggers a `.txt` download with the transcript content.
- [ ] "MD File" triggers a `.md` download with frontmatter (when multi-speaker) or plain header (single-speaker).
- [ ] When "Prompt for filename" is checked, file-save modes show a filename dialog before downloading.
- [ ] When "Prompt for filename" is unchecked, files are saved immediately with the auto-generated name.
- [ ] The selected mode and filename-prompt setting persist across page reloads.
- [ ] Confirmation feedback is shown after each transfer action.
- [ ] "VS Code" appears grayed out / disabled with a "coming soon" note.

---

## Implementation Order

| Phase | Feature | Depends on |
|-------|---------|------------|
| **A** | System audio capture (`start_system_audio`) | Nothing |
| **B** | Settings UI (speaker cards, toggles) | Nothing |
| **C** | Dual-session architecture | A + B |
| **D** | Speaker-labeled transcript (tags + timestamps) | C |
| **E** | Transfer / export combo button | Nothing (can be built in parallel with A–D) |

Phases A, B, and E are independent and can be built in any order or in parallel. C requires A and B. D requires C.

---

## Non-goals (out of scope)

- **AI-based diarization** — not needed when audio sources are physically separated. May revisit if single-mic multi-speaker is desired later.
- **Voice fingerprinting / profiles** — future feature, not needed for two-stream separation.
- **Audio recording / storage** — Parley does not save audio files in this phase. Only transcript text is captured.
- **VS Code extension** — documented above as a future transfer mode but not built in this round.
- **macOS / Firefox / Safari system audio** — documented as limitations, not addressed.

---

## Risk & open questions

1. **`getDisplayMedia` requires user interaction** — the browser will show a screen-share dialog every time. There's no way to suppress this. Users will need to click through it each time they start a multi-speaker session. This is a UX friction point but not solvable at the app level.

2. **Two tokens = two API calls at startup** — adds ~1–2 seconds to the connection flow. Both token fetches can run in parallel (`join!`).

3. **Overlapping speech** — both speakers can talk simultaneously. Each has their own current-turn box so both partials are always visible. No information is lost.

4. **System audio captures ALL system sound** — not just Zoom. If the user has music playing, that goes into the remote speaker's stream. This is inherent to `getDisplayMedia` system audio and should be noted in the UI ("Tip: mute other applications for clean capture").
