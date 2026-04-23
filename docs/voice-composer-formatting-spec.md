# Voice Composer Formatting Integration

**Status:** Draft
**Type:** Specification
**Audience:** Both
**Date:** 2026-04-22

## 1. Problem

The Conversation view's voice composer ([`src/ui/conversation.rs`](../src/ui/conversation.rs)) currently submits the *raw* AssemblyAI transcript as the user turn — lowercased, unpunctuated, and without paragraph breaks for long inputs. The Transcribe view, in contrast, runs every chunk through a Haiku-based formatting pass ([`check_formatting` / `check_formatting_with_context` in src/ui/app.rs](../src/ui/app.rs)) that adds punctuation, capitalization, paragraph breaks, and joins acronyms. The output is dramatically more readable.

Voice-mode conversation deserves the same quality, both for what the *user* sees in the live transcript bubble and for what the *LLM* receives as the user turn.

Two follow-on problems compound this:

1. The formatting logic in `app.rs` is a 600-ish-line slab tangled with the Transcribe view's chunking, change-detection, and cost tracking. It's not reusable as written.
2. The voice composer needs to make a judgment call: do we wait for the formatter before sending the turn (slow, accurate), send the raw text immediately and update later (fast, but the LLM has already started on a malformed prompt), or some hybrid?

This spec extracts the formatter into a reusable hook, integrates it into the voice composer, and resolves the latency-vs-quality question.

## 2. Goals & Non-Goals

### Goals

- **G1.** Extract the Haiku-based formatting pipeline out of `src/ui/app.rs` into a hook (`use_text_formatter`) usable by both the Transcribe view and the Conversation view.
- **G2.** The voice composer's live transcript bubble shows formatted text, updated as the formatter returns.
- **G3.** The user turn submitted to the conversation orchestrator uses the *formatted* text, not the raw transcript.
- **G4.** Send-turn latency does not regress unacceptably. Define "unacceptable" precisely (§3.5).
- **G5.** All existing Transcribe-view behavior (cost display, change-detection, multi-speaker prompt mode, paragraph chunking) continues to work, driven by the same hook.

### Non-Goals

- **NG1.** Replacing Haiku with a different formatter, or adding local rule-based formatting. The Haiku pipeline stays as-is.
- **NG2.** Streaming formatter output. The formatter is one-shot per chunk; it returns a complete formatted string or `None`.
- **NG3.** Formatting LLM *responses* in the conversation view. AI responses arrive already well-formatted; this spec only touches the user-input path.
- **NG4.** Persisting formatted-vs-raw in the session file. The session stores only the final formatted text — the raw transcript is a UI-time artifact.

## 3. Design

### 3.1 Hook Extraction

Create `src/ui/formatter.rs` exposing one hook:

```rust
/// State + actions for the Haiku-based formatter.
#[derive(Clone, Copy)]
pub struct UseTextFormatter {
    /// The most recently formatted full text. Updates as chunks
    /// are formatted. Initially empty.
    pub formatted: Signal<String>,
    /// True while a format request is in flight. Multiple in-flight
    /// requests are coalesced — see §3.3.
    pub in_flight: Signal<bool>,
    /// Cumulative input/output tokens spent on formatting in this
    /// hook's lifetime. For cost display.
    pub usage: Signal<FormatUsage>,
}

impl UseTextFormatter {
    /// Replace the current text. Triggers re-formatting of the
    /// changed tail in the background. Idempotent if `text` matches
    /// the last call.
    pub fn set_text(&self, text: String);
    /// Wait for any in-flight formatting to complete and return
    /// the final formatted string. Use when the caller needs a
    /// final, settled result (e.g., on send-turn).
    pub async fn settle(&self) -> String;
    /// Clear all state. Use when starting a new turn / new chunk
    /// boundary.
    pub fn reset(&self);
}

pub fn use_text_formatter(opts: FormatterOpts) -> UseTextFormatter;

#[derive(Clone, Copy)]
pub struct FormatterOpts {
    pub multi_speaker: bool,
    /// Anthropic model id; defaults to Haiku 4.5.
    pub model: &'static str,
    /// Minimum interval between formatting requests for the same
    /// growing text. Default 750ms.
    pub debounce_ms: u32,
}
```

`set_text` is the universal entry point. The hook internally:

- Splits the new text into [paragraph block + trailing list blocks] chunks (the existing logic in `app.rs` line ~220 onwards).
- Identifies the *last unformatted chunk* by comparing against the previously-formatted text.
- Sends only the editable tail to `/format` with the rest as `context`.
- Splices the response back into `formatted`.

This is exactly the existing `check_formatting_with_context` flow; the extraction is a refactor, not a redesign.

### 3.2 Voice Composer Integration

In the Conversation view's voice mode:

```mermaid
sequenceDiagram
    participant Mic
    participant AAI as AssemblyAI WS
    participant Hook as use_text_formatter
    participant UI as Voice transcript bubble
    participant Send as Send Turn

    Mic->>AAI: audio frames
    AAI-->>Hook: partial transcript (raw)
    Hook->>Hook: debounce 750ms
    Hook->>Proxy: POST /format
    Proxy-->>Hook: formatted text
    Hook-->>UI: formatted signal updated
    Note over Mic,UI: User keeps speaking; loop
    UI->>Send: click Send
    Send->>Hook: settle().await
    Hook-->>Send: final formatted string
    Send->>Orchestrator: submit_user_turn(formatted)
```

The voice composer stops storing a separate `transcript: Signal<String>`. Instead it owns a `UseTextFormatter` and feeds raw AssemblyAI text into `set_text`. The transcript bubble renders `formatter.formatted()`. The send action calls `formatter.settle().await`.

### 3.3 In-Flight Coalescing

While a `/format` request is in flight, additional `set_text` calls update an internal "pending text" but do not fire a new request. When the in-flight request returns, the hook compares the response against the current pending text:

- If the pending text starts with the text that was sent (i.e., the user kept speaking but didn't go back and edit), the formatted result is spliced in and a new request fires for the new tail (subject to debounce).
- If the pending text diverges (the user backed up — relevant for Transcribe-view edits, not voice), the formatted result is discarded and a fresh request fires for the full pending text.

Voice composer never edits backwards (AssemblyAI streams forward-only), so the divergence path matters only for Transcribe view re-use.

### 3.4 Reset Boundaries

The formatter's `reset()` is called:

- When a new turn begins (Conversation view) or a new chunk boundary is committed (Transcribe view).
- When the user switches between Voice and Type modes.
- When a session is loaded.

Reset is local to one `UseTextFormatter` instance. Multiple hook instances on a page are allowed (Transcribe view and Conversation view each have their own).

### 3.5 Latency Budget on Send-Turn

When the user clicks Send while a format request is in flight, `settle()` waits. Define the budget:

| Condition | Behavior |
|---|---|
| In-flight request, no pending edits | Wait up to **2000ms** for it to complete; on timeout, fall back to last-known formatted text. |
| In-flight request, pending edits | Wait up to **2000ms** for the in-flight request, then fire one *final* request with full text and wait up to **3000ms**; on timeout, fall back to last-known formatted text + raw appended tail. |
| No in-flight request, formatted is current | Use immediately. |
| No formatter response ever received (e.g., proxy down, no Anthropic key) | Use raw text. Log a warning. The conversation still works. |

The total worst-case added latency is 5 seconds, which is acceptable because (a) it's bounded, (b) Haiku usually returns under 1.5s for paragraph-sized inputs, and (c) the user just finished speaking and is mentally prepared for a brief processing beat. If empirically the wait is too long, lower the budgets or move toward an "optimistic send" model (NG below) in a follow-up spec.

A previously considered alternative — *optimistic send* (submit raw text immediately, send a "user turn correction" message later) — is rejected for v1 because the LLM may have already produced tokens conditioned on the malformed prompt, and reconciling that is far worse than a 1–2 second wait.

### 3.6 Multi-Speaker Mode

Conversation voice mode runs as **single-speaker** (just the user). Pass `multi_speaker: false` from the conversation view. Transcribe view continues to pass `true` when its multi-speaker toggle is on.

### 3.7 Cost Display

`UseTextFormatter::usage` exposes cumulative tokens. Transcribe view continues to display its existing cost summary. Conversation view does *not* surface formatter cost in v1 — it's a small fraction of the LLM cost and not worth the UI real estate yet.

## 4. Implementation Plan

The work is sequential; do not parallelize across these items.

1. **Extract hook (no behavior change).** Create `src/ui/formatter.rs` with `use_text_formatter`. Move `FormatResult`, `check_formatting`, `check_formatting_with_context`, and the chunk-detection logic out of `app.rs` and into the hook. Wire the Transcribe view to use the hook. Verify the Transcribe view behaves identically (manual test: paragraph-aware chunking, multi-speaker mode, cost display all work).
2. **Add `settle()` and the debounce/coalesce machinery.** Currently `app.rs` triggers formatting on a debounced effect over the transcript signal — keep that behavior, but expose `settle()` as the new external entry point. Verify Transcribe view still works.
3. **Wire voice composer.** In `src/ui/conversation.rs`'s voice mode, replace the raw-transcript pipeline with a `UseTextFormatter` instance. Feed AssemblyAI partials into `set_text`. Render `formatter.formatted()` in the transcript bubble. On send, `formatter.settle().await` and submit the result.
4. **Reset boundaries.** Hook `reset()` into mode switch (`Mode::Voice ↔ Mode::Type`), session load, and post-send.
5. **Failure-mode handling.** If `/format` returns an error or times out, the hook continues to expose the latest known good `formatted`. Voice composer's send falls back to raw text per §3.5 row 4.

## 5. Test Plan

| Component | Tests |
|---|---|
| `use_text_formatter` (extracted hook) | (1) `set_text` then await `settle()` → returns formatted text. (2) Two rapid `set_text` calls within debounce → one `/format` request fires (verified via mock). (3) `set_text` while in-flight → in-flight result spliced, new request queued. (4) `reset()` clears `formatted` and aborts any in-flight handling of the response. (5) `/format` returns `{changed: false}` → `formatted` mirrors input. (6) `/format` returns error → `formatted` keeps last value, `in_flight` clears. |
| Transcribe view regression | Manual: full chunking + multi-speaker + cost display all behave as before extraction. No automated test (matches current convention for the Transcribe view). |
| Voice composer integration | (1) Speak a sentence, wait, send → submitted text is formatted. (2) Speak a sentence, send immediately → `settle()` waits for the in-flight request and submits formatted text within 2s. (3) Mock `/format` to never respond, send → after 5s combined timeout, raw text submitted with a warning logged. (4) Switch from Voice to Type mid-formatting → hook resets; no stale formatted text leaks into the type-mode textarea. |
| Latency budget | Mock `/format` with controlled latency: 500ms, 1500ms, 3000ms, never. Verify `settle()` returns within budget for each case. |

## 6. Migration & Compatibility

- `app.rs` shrinks by ~600 lines. Diff should be predominantly *moves*, not rewrites — minimize behavior change in step 1.
- Session files: unchanged. Conversation user turns store the formatted text; today they store the raw AssemblyAI transcript. This is a content quality improvement, not a schema change.
- `/format` proxy endpoint: unchanged.
- No new persistence, no new wire protocol.

## 7. Open Questions

1. **Sentence-by-sentence formatter?** The current chunking approach formats whole paragraphs. For voice composer, would a per-sentence formatter feel more responsive? Probably not — Haiku per-sentence is wasteful and the paragraph context is what makes formatting good. Keep paragraph chunking.
2. **User-visible "formatting…" indicator.** Should the voice transcript bubble show a subtle indicator while `in_flight` is true? Lean yes — small italic "…" or animated dots — but defer the visual spec until step 3 of implementation; it's a small follow-up.
3. **Anthropic key absent.** Conversation mode requires an Anthropic key already (for the LLM). If the user has Anthropic configured for the LLM but the formatter still fails (e.g., model id wrong), behavior is "fall back to raw text." Document but do not engineer a separate fallback formatter.

## 8. Out-of-Scope Reminders

- Local/offline formatting fallback (NG1).
- Streaming formatter output (NG2).
- Formatting LLM responses (NG3).
- Storing both raw and formatted in the session file (NG4).
- Optimistic send with later correction (rejected, §3.5).
