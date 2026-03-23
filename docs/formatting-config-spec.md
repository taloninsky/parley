# LLM Formatting Configuration — Specification

> Status: **Draft**
> Author: Gavin + Copilot
> Date: 2026-03-23

---

## Overview

This spec makes the LLM-powered transcript formatting fully configurable:

1. **Model selection** — choose between Haiku 4.5 (fast/cheap) and Sonnet 4.6 (higher quality)
2. **Trigger strategy** — control when auto-formatting fires (every turn, every Nth turn, on stop, manual only, or off)
3. **Scope control** — how many trailing chunks to reformat per pass
4. **On-demand reformat button** — one-click full or partial reformat using the selected model

---

## 1. Model Selection

### Current behavior

The proxy hardcodes `"claude-haiku-4-5-20251001"` in the Anthropic API call.

### Change

Add a `model` field to `FormatRequest`. The proxy passes it through to the Anthropic `model` parameter.

#### 1.1 Proxy: `FormatRequest`

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
    /// Anthropic model ID. Defaults to Haiku 4.5 if omitted.
    #[serde(default = "default_model")]
    model: String,
}

fn default_model() -> String {
    "claude-haiku-4-5-20251001".to_string()
}
```

The `format_text` handler uses `body.model` instead of the hardcoded string:

```rust
let payload = serde_json::json!({
    "model": body.model,
    "max_tokens": 4096,
    // ...
});
```

#### 1.2 Frontend model signal

File: `src/ui/app.rs`

```rust
let mut format_model = use_signal(||
    load("parley_format_model").unwrap_or_else(|| "claude-haiku-4-5-20251001".to_string())
);
```

The two supported values:

| Display label | Model ID |
|---------------|----------|
| Haiku 4.5 | `claude-haiku-4-5-20251001` |
| Sonnet 4.6 | `claude-sonnet-4-6-20250514` |

Persisted via cookie `parley_format_model`.

#### 1.3 Settings UI

A dropdown in the **Formatting** section of the settings drawer:

```
Model: [Haiku 4.5 ▾]
```

---

## 2. Trigger Strategy

### Current behavior

`npc.set(true)` fires on **every** turn commit for both speakers, meaning every single turn ending triggers an Anthropic API call.

### Change

Introduce a trigger strategy enum and a turn counter.

#### 2.1 Trigger enum

```rust
#[derive(Clone, Copy, PartialEq)]
enum FormatTrigger {
    EveryTurn,
    EveryNth,
    OnStop,
    Manual,
    Off,
}
```

Cookie key: `parley_format_trigger`. Values: `"every-turn"`, `"every-nth"`, `"on-stop"`, `"manual"`, `"off"`.

**Default: `EveryNth`** (with N=3).

#### 2.2 Turn counter

An `Rc<Cell<u32>>` initialized to `0` at recording start. Incremented on every turn commit (both speakers). When the strategy is `EveryNth`, `npc.set(true)` fires only when `counter % N == 0`.

#### 2.3 N value

```rust
let mut format_nth = use_signal(||
    load("parley_format_nth")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(3)
);
```

Cookie key: `parley_format_nth`. **Default: 3.**

#### 2.4 Logic at turn commit sites

Both turn-commit callbacks (speaker 1 around line 788, speaker 2 around line 916) currently do:

```rust
if !(anthropic_key)().is_empty() {
    npc.set(true);
}
```

Replace with:

```rust
if !(anthropic_key)().is_empty() {
    turn_commit_counter.set(turn_commit_counter.get() + 1);
    let trigger = (format_trigger)();
    match trigger {
        FormatTrigger::EveryTurn => npc.set(true),
        FormatTrigger::EveryNth => {
            if turn_commit_counter.get() % (format_nth)() == 0 {
                npc.set(true);
            }
        }
        // OnStop, Manual, Off — don't trigger here
        _ => {}
    }
}
```

#### 2.5 On-stop formatting

The existing on-stop `check_formatting` call (around line 1400) should be gated:

```rust
let trigger = (format_trigger)();
if !akey.is_empty() && matches!(trigger,
    FormatTrigger::EveryTurn | FormatTrigger::EveryNth | FormatTrigger::OnStop
) {
    // ... existing on-stop format call
}
```

This means `Manual` and `Off` skip even the on-stop pass.

#### 2.6 Settings UI

```
Auto-format: [Every 3rd turn ▾]
```

Dropdown options:
- Every turn
- Every Nth turn → reveals an adjacent number input for N
- On stop only
- Manual only
- Off

---

## 3. Scope Control (Reformat Depth)

### Current behavior

`check_formatting` uses a fixed window: in single-speaker mode, up to 3 chunks with 2 editable; in multi-speaker, up to 6 chunks with 4 editable.

### Change

Make the number of editable chunks configurable. The context window stays at `editable + context_padding` (currently context_padding is 1 in single, 2 in multi).

#### 3.1 Signal

```rust
let mut format_depth = use_signal(||
    load("parley_format_depth")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(2)
);
```

Cookie key: `parley_format_depth`. **Default: 2.**

#### 3.2 Replacing the hardcoded window

In `check_formatting`, the current logic:

```rust
let (max_win, max_editable) = if multi_speaker { (6, 4) } else { (3, 2) };
```

Becomes parameterized — `check_formatting` gains a `depth: usize` parameter:

```rust
async fn check_formatting(
    anthropic_key: &str,
    full_transcript: &str,
    multi_speaker: bool,
    model: &str,
    depth: usize,
) -> Option<String>
```

Inside:

```rust
let context_padding: usize = if multi_speaker { 2 } else { 1 };
let max_editable = depth;
let max_win = depth + context_padding;
```

#### 3.3 Full-transcript mode

When depth is `0` (or a sentinel like `usize::MAX`), the entire transcript is editable with no context split. This is used by the Reformat button. Implementation: skip the chunking/windowing and send the entire transcript as the editable text with empty context.

#### 3.4 Settings UI

```
Reformat depth: [2 ▾]  chunks
```

Dropdown or small number input. Reasonable range: 1–6.

---

## 4. On-Demand Reformat Button

### Behavior

A **¶ Reformat** button in the bottom toolbar (next to Stop / Copy / Clear). Visible whenever an Anthropic API key is configured, regardless of recording state.

When clicked:
1. Calls `check_formatting` with the **entire transcript** as editable (depth = full).
2. Uses the **currently selected model** from the dropdown.
3. Shows a brief loading indicator (e.g., the button text changes to "Reformatting…" and is disabled).
4. On completion, splices the formatted text back into the transcript signal, preserving cursor position.

### Why full-transcript

The point of the manual button is to get one coherent pass over everything — fix paragraph breaks that the incremental windowed passes may have gotten wrong, ensure consistent list formatting, etc.

### Implementation

```rust
let on_reformat = move |_: Event<MouseData>| {
    let key = (anthropic_key)();
    let model = (format_model)();
    let multi = (speaker2_enabled)();
    let mut t = transcript.clone();
    spawn(async move {
        let text = (t)();
        if !text.is_empty() && !key.is_empty() {
            // depth 0 = full transcript
            if let Some(formatted) = check_formatting(&key, &text, multi, &model, 0).await {
                let cursor = get_cursor();
                t.set(formatted);
                if let Some((s, e)) = cursor {
                    restore_cursor(s, e);
                }
            }
        }
    });
};
```

### Button placement

In the bottom action bar, after the existing buttons:

```
[ ■ Stop ]  [ Copy ▾ ]  [ Clear ]  [ ¶ Reformat ]
```

Styled to match the existing button theme. Disabled (greyed out) when no Anthropic key is set or when the transcript is empty.

---

## 5. `check_formatting` — Updated Signature

The function gains two new parameters:

```rust
async fn check_formatting(
    anthropic_key: &str,
    full_transcript: &str,
    multi_speaker: bool,
    model: &str,        // NEW: Anthropic model ID
    depth: usize,       // NEW: 0 = full transcript, N = last N editable chunks
) -> Option<String>
```

The `model` is included in the JSON body sent to the proxy:

```rust
let body = serde_json::json!({
    "anthropic_key": anthropic_key,
    "context": context_text,
    "text": editable_text,
    "multi_speaker": multi_speaker,
    "model": model,
});
```

When `depth == 0`, skip chunking entirely:

```rust
if depth == 0 {
    // Full-transcript mode: everything is editable, no context
    let editable_text = full_transcript.to_string();
    let context_text = String::new();
    let prefix = String::new();
    // ... send to proxy and return
}
```

All existing call sites pass the model and depth from their respective signals.

---

## 6. Settings UI Layout

New **Formatting** section in the settings drawer, placed after the Anthropic API key field:

```
── Formatting ─────────────────────────────────
Model:           [Haiku 4.5 ▾]
Auto-format:     [Every 3rd turn ▾]
  N:             [3]          (visible only when "Every Nth turn" selected)
Reformat depth:  [2] chunks
```

All values persisted via cookies:

| Cookie key | Default | Type |
|------------|---------|------|
| `parley_format_model` | `claude-haiku-4-5-20251001` | string |
| `parley_format_trigger` | `every-nth` | string |
| `parley_format_nth` | `3` | u32 |
| `parley_format_depth` | `2` | usize |

---

## 7. Summary of Changes

| File | Change |
|------|--------|
| `proxy/src/main.rs` | Add `model` field to `FormatRequest`; use `body.model` in API call |
| `src/ui/app.rs` | New signals: `format_model`, `format_trigger`, `format_nth`, `format_depth` |
| `src/ui/app.rs` | `check_formatting` gains `model` and `depth` params |
| `src/ui/app.rs` | Turn commit callbacks gated by trigger strategy + counter |
| `src/ui/app.rs` | On-stop formatting gated by trigger strategy |
| `src/ui/app.rs` | New `¶ Reformat` button in bottom toolbar |
| `src/ui/app.rs` | New Formatting section in settings drawer |

### Done criteria

- [ ] Model dropdown switches between Haiku 4.5 and Sonnet 4.6; choice persists across reloads.
- [ ] Auto-format trigger dropdown works: every turn, every Nth, on stop, manual, off.
- [ ] Every-Nth correctly counts turn commits and only fires on every Nth.
- [ ] Reformat depth controls how many trailing chunks are sent as editable.
- [ ] ¶ Reformat button reformats the entire transcript using the selected model.
- [ ] All settings persist via cookies.
- [ ] Existing single-speaker and multi-speaker formatting still works correctly.
