# LLM Formatting Configuration — Specification

> Status: **Implemented**
> Author: Gavin + Copilot
> Date: 2026-03-23

---

## Overview

This spec makes the LLM-powered transcript formatting fully configurable:

1. **Model selection** — choose between Haiku 4.5 (fast/cheap) and Sonnet 4.6 (higher quality) for incremental auto-formatting
2. **Auto-format toggle** — checkbox to enable/disable auto-formatting every N turns, with configurable N
3. **Format on stop** — optional full-transcript reformat when recording stops, always using Sonnet 4.6
4. **Scope control** — configurable reformat depth (editable chunks) and additional visibility depth (context chunks)
5. **On-demand reformat button** — one-click full reformat using Sonnet 4.6
6. **Graceful stop** — stop capture, force endpoint, wait for final formatted transcript from STT, then terminate

---

## 1. Model Selection

A `model` field on `FormatRequest` (proxy) is passed through to the Anthropic API.

### 1.1 Proxy: `FormatRequest`

File: `proxy/src/main.rs`

```rust
#[derive(Deserialize)]
struct FormatRequest {
    anthropic_key: String,
    #[serde(default)]
    context: String,
    text: String,
    #[serde(default)]
    multi_speaker: bool,
    #[serde(default = "default_model")]
    model: String,
}
```

### 1.2 Frontend model signal

```rust
let mut format_model = use_signal(||
    load("parley_format_model").unwrap_or_else(|| "claude-haiku-4-5-20251001".to_string())
);
```

| Display label | Model ID |
|---------------|----------|
| Haiku 4.5 | `claude-haiku-4-5-20251001` |
| Sonnet 4.6 | `claude-sonnet-4-6-20250514` |

Cookie: `parley_format_model`.

### 1.3 Settings UI

Dropdown labeled **"Incremental auto-format model"** in the Formatting section of the settings drawer.

---

## 2. Auto-Format Trigger

A checkbox **"Auto-format every N turns"** with an adjacent number input for N.

### 2.1 Signals

```rust
let mut auto_format_enabled = use_signal(||
    load("parley_auto_format").map(|s| s == "true").unwrap_or(true)
);
let mut format_nth = use_signal(||
    load("parley_format_nth").and_then(|s| s.parse::<u32>().ok()).unwrap_or(3)
);
```

Cookies: `parley_auto_format` (default `true`), `parley_format_nth` (default `3`, min `1`).

### 2.2 Logic at turn commit sites

Both speaker callbacks use a shared `turn_commit_counter: Rc<Cell<u32>>`. On each turn commit:

```rust
if !(anthropic_key)().is_empty() && (auto_format_enabled)() {
    tcc.set(tcc.get() + 1);
    if tcc.get() % (format_nth)() == 0 {
        npc.set(true);  // trigger paragraph check
    }
}
```

When `auto_format_enabled` is false, no automatic formatting is triggered. The manual Reformat button and format-on-stop remain available.

---

## 3. Scope Control

### 3.1 Reformat depth (editable chunks)

```rust
let mut format_depth = use_signal(||
    load("parley_format_depth").and_then(|s| s.parse::<usize>().ok()).unwrap_or(2)
);
```

Cookie: `parley_format_depth`. Default: `2`.

### 3.2 Additional visibility depth (context chunks)

```rust
let mut format_context_depth = use_signal(||
    load("parley_format_context_depth").and_then(|s| s.parse::<usize>().ok()).unwrap_or(1)
);
```

Cookie: `parley_format_context_depth`. Default: `1`.

### 3.3 check_formatting signature

```rust
async fn check_formatting(
    anthropic_key: &str,
    full_transcript: &str,
    multi_speaker: bool,
    model: &str,
    depth: usize,          // 0 = full transcript, N = last N editable chunks
    context_depth: usize,  // additional read-only context chunks before the editable window
) -> Option<FormatResult>
```

Inside:
```rust
let max_editable = depth;
let max_win = depth + context_depth;
```

When `depth == 0`, the entire transcript is editable with no context split (used by Reformat button and format-on-stop full pass).

### 3.4 Settings UI

```
Reformat depth (chunks):             [2]
Additional visibility depth (chunks): [1]
```

Small hint text: *"Depth 0 = full transcript. Visibility adds read-only context chunks before the editable window."*

---

## 4. Format on Stop (Full Sonnet Pass)

### 4.1 Signal

```rust
let mut format_on_stop = use_signal(||
    load("parley_format_on_stop").map(|s| s == "true").unwrap_or(true)
);
```

Cookie: `parley_format_on_stop`. Default: `true`.

### 4.2 Behavior

When recording stops and this checkbox is enabled, after any incremental auto-format pass completes, a **full-transcript** formatting pass is triggered using Sonnet 4.6 (`claude-sonnet-4-6-20250514`) with `depth=0, context_depth=0`.

If auto-format is disabled but format-on-stop is enabled, only the full Sonnet pass runs (the incremental pass is skipped).

### 4.3 Settings UI

Checkbox: **"Also format on stop (full pass, Sonnet 4.6)"**

---

## 5. On-Demand Reformat Button

A **"¶ Reformat"** button in the bottom toolbar. Calls `check_formatting` with `depth=0` (full transcript) using Sonnet 4.6. Disabled when no Anthropic key is set, transcript is empty, or recording is in Stopping state.

---

## 6. Graceful Stop

### 6.1 RecState enum

```rust
enum RecState { Idle, Recording, Stopping, Stopped }
```

### 6.2 Stop flow

When the user clicks Stop:

1. Set `RecState::Stopping` — disables all buttons
2. **Stop audio capture** on both speakers (no more audio sent to STT)
3. **Force endpoint** on all active sessions (causes STT to finalize the current turn)
4. **Wait for final formatted transcript**: poll shared `turn_is_formatted` flags (set by session callbacks when AssemblyAI responds with `turn_is_formatted: true`). Only waits for sessions that had non-empty partials. 5-second safety timeout.
5. **Terminate** sessions (send `{"type": "Terminate"}`)
6. **Flush partials** to transcript (single-speaker) or live zone (multi-speaker)
7. **Graduate all live words** to transcript (multi-speaker)
8. **Run formatting** — incremental pass (if auto-format enabled) then full Sonnet pass (if format-on-stop enabled)
9. Set `RecState::Stopped`

### 6.3 Formatted flag mechanism

Each session callback tracks `turn_is_formatted` from AssemblyAI Turn events via shared `Rc<Cell<bool>>`:

```rust
let formatted_flag1: Rc<Cell<bool>> = Rc::new(Cell::new(false));  // speaker 1
let formatted_flag2: Rc<Cell<bool>> = Rc::new(Cell::new(false));  // speaker 2
```

Exposed via signals: `turn_is_formatted1_shared`, `turn_is_formatted2_shared`.

In each callback:
```rust
ff.set(is_formatted);
```

In on_stop, before calling force_endpoint:
```rust
// Reset flags for sessions with pending partials
if s1_has_partial { formatted_flag1.set(false); }
if s2_has_partial { formatted_flag2.set(false); }
// Call force_endpoint...
// Poll until flags are true (or 5s timeout)
```

---

## 7. Settings UI Layout

**Formatting** section in settings drawer (visible when Anthropic key is set):

```
── Formatting ──────────────────────────────────
☑ Auto-format every [3] turns
Incremental auto-format model:  [Haiku 4.5 ▾]
Reformat depth (chunks):        [2]
Additional visibility depth:    [1]
  Depth 0 = full transcript. Visibility adds read-only context
  chunks before the editable window.
☑ Also format on stop (full pass, Sonnet 4.6)
```

---

## 8. Cookie Summary

| Cookie key | Default | Type |
|------------|---------|------|
| `parley_format_model` | `claude-haiku-4-5-20251001` | string |
| `parley_auto_format` | `true` | bool |
| `parley_format_nth` | `3` | u32 |
| `parley_format_depth` | `2` | usize |
| `parley_format_context_depth` | `1` | usize |
| `parley_format_on_stop` | `true` | bool |

---

## 9. Summary of Changes

| File | Change |
|------|--------|
| `proxy/src/main.rs` | `model` field on `FormatRequest`; passed through to Anthropic API |
| `src/ui/app.rs` | Signals: `format_model`, `auto_format_enabled`, `format_nth`, `format_depth`, `format_context_depth`, `format_on_stop` |
| `src/ui/app.rs` | `check_formatting` takes 6 params: key, transcript, multi, model, depth, context_depth |
| `src/ui/app.rs` | Turn commit callbacks gated by `auto_format_enabled` + `format_nth` modulo |
| `src/ui/app.rs` | Graceful stop: stop capture → force endpoint → wait for formatted flag → terminate → flush → format |
| `src/ui/app.rs` | `turn_is_formatted` flags shared between session callbacks and on_stop |
| `src/ui/app.rs` | `RecState::Stopping` variant; all buttons disabled during stop |
| `src/ui/app.rs` | `¶ Reformat` button (full Sonnet 4.6 pass) |
| `src/ui/app.rs` | Formatting section in settings drawer |
