# Expressive Annotation (Audio-Tag Injection)

**Status:** Draft
**Type:** Specification
**Audience:** Both
**Date:** 2026-04-23

## 1. Problem

ElevenLabs v3 and xAI TTS accept inline **audio tags** that direct prosody, emotion, and pacing. ElevenLabs-style tags include `[laughs]`, `[whispers]`, `[sighs]`, `[excited]`, `[curious]`, `[serious]`, and `[long pause]`; xAI's current native point tags include `[pause]`, `[long-pause]`, `[laugh]`, and `[sigh]`, with scoped style wrappers deferred to [xAI TTS Prosody Improvement 1](xai-improve-1.md). Bare narrative text without tags reads flat. ElevenLabs' own "Enhance" feature in their TTS playground demonstrates the lift: same model, same voice, but annotated text sounds *acted* rather than read.

The orchestrator currently sends raw text to the active TTS provider. We want a small, pluggable pass that runs **between the chunker and the TTS provider** to inject tags appropriate for each chunk. The legal tag surface is provider-owned: every `TtsProvider` advertises its own expression instruction and translates only the tags/spans that model can render safely.

This pass must be:

- **Provider-aware.** Only runs when the active provider supplies an expression instruction. Skips for ElevenLabs v2, Flash, OpenAI TTS, etc.
- **Concurrent-friendly.** The annotator for chunk N+1 runs while the synthesizer for chunk N is in flight. Serial annotation would push TTFA past acceptable.
- **Cheap.** Per-chunk LLM call to a Haiku-class model. Tens of milliseconds; cents per turn.
- **Failure-tolerant.** If the annotator errors or times out, the chunk is sent to TTS with its original text. The user gets less expressive audio, not no audio.
- **Configurable.** The default annotator is a system-prompted Haiku call, but the trait must permit substituting a different model, a local function, or a future per-persona prompt.

## 2. Goals & Non-Goals

### Goals

- **G1.** Define an `Annotator` trait and a default Haiku-backed implementation.
- **G2.** Place the annotator in the chunk pipeline so it runs concurrently for the *next* chunk while the *current* chunk synthesizes.
- **G3.** Provider-aware enablement based on whether [`TtsProvider::expression_tag_instruction()`](paragraph-tts-chunking-spec.md#32-provider-trait) returns an instruction.
- **G4.** Inter-list-item pacing handled here: ellipses for v3, `<break time="0.3s"/>` for tag-aware providers that prefer SSML-like syntax, plain text for unsupported providers.
- **G5.** Graceful degradation on annotator failure.
- **G6.** Test coverage for tag injection, no-op pass-through, fallback on error, and ordering preservation under concurrency.

### Non-Goals

- **NG1.** Per-persona annotator prompts. v1 uses a single global default. Future work via [persona CRUD](persona-crud-spec.md).
- **NG2.** Annotation of the user's *input* (the user's spoken or typed message). Annotation applies only to LLM output bound for TTS.
- **NG3.** Voice-cloning, accent, or speaker-identity annotation. Out of scope; the chunk's voice is already chosen at the persona/model layer.
- **NG4.** Word-level timing or alignment annotations. Belongs to a future spec on visual word-by-word display.
- **NG5.** Streaming the annotator (token-streamed tags). v1 calls the annotator with the full chunk text and waits for the full annotated response. Annotator latency is small enough relative to TTS that streaming the annotator adds complexity without payoff.
- **NG6.** Editing the chunker's chunk boundaries. The annotator is text-in/text-out; it must not split, merge, or reorder chunks.

## 3. Architecture

### 3.1 Pipeline Placement

```mermaid
flowchart LR
    Planner[ChunkPlanner] -- ReleasedChunk --> AnnotatorQueue
    AnnotatorQueue -- Annotated --> SingleFlight[Single-flight TTS]
    SingleFlight --> Provider[TtsProvider]
    Provider --> Splicer[SilenceSplicer]
    Splicer --> Hub[TtsHub]
```

Per turn, the orchestrator runs an `AnnotatorQueue`:

1. Receive `ReleasedChunk` from `ChunkPlanner`.
2. If `provider.expression_tag_instruction()` returns `None`, pass the chunk through unchanged.
3. Otherwise, spawn an annotation task for the chunk and store its `JoinHandle` (or `oneshot::Receiver`) keyed by `chunk.index`.
4. Dispatch chunks to TTS in **strictly increasing index order**: chunk N+1 only dispatches after chunk N has been sent to TTS, regardless of which annotation finished first.
5. While chunk N is being synthesized (which takes longer than annotation), chunk N+1's annotation task runs concurrently and is typically already done by the time chunk N's synthesis finishes.

This gives us "annotator hidden behind synthesis time" without blocking the chunker.

### 3.2 Annotator Trait

```rust
#[async_trait]
pub trait Annotator: Send + Sync {
    /// Annotate one chunk's text with provider-appropriate expressive
    /// markers. Returns the annotated text.
    ///
    /// On error, the orchestrator falls back to the input text unchanged.
    async fn annotate(
        &self,
        text: &str,
        ctx: AnnotationContext,
    ) -> Result<String, AnnotatorError>;
}

#[derive(Debug, Clone)]
pub struct AnnotationContext {
    /// The provider that will synthesize this text. Used by the
    /// annotator to choose tag syntax.
    pub provider_kind: ProviderKind,
    /// Zero-based chunk index in the turn.
    pub chunk_index: u32,
    /// True for the final chunk of the turn.
    pub final_for_turn: bool,
    /// The persona-level tone hint, if any (e.g., "warm", "clinical",
    /// "playful"). Optional input to the annotator's system prompt.
    pub tone_hint: Option<String>,
}

pub enum ProviderKind {
    ElevenLabsV3,
    GrokSpeech,
    /// Tag-aware provider not in the enum yet. Annotator should default
    /// to ElevenLabs v3 syntax (audio tags + ellipses).
    OtherTagAware,
}
```

`AnnotatorError` is an opaque error type; the orchestrator only branches on success/failure, never inspects the variant.

### 3.3 Default Annotator: `HaikuAnnotator`

The default implementation calls a Haiku-class model (Claude Haiku 4 or equivalent) via the existing LLM proxy infrastructure with a focused system prompt.

The system prompt is modeled on ElevenLabs' "Enhance" prompt — direct injection of audio tags, no rewriting of words, no adding or removing content, and a list of legal tags. Approximate shape:

```
You are an audio direction assistant. The user provides one short
passage of text intended for text-to-speech narration. Your job is
to insert audio tags that direct prosody, emotion, and pacing.

DO:
- Insert tags from the legal list, in square brackets, inline at the
  point in the text where the effect should occur.
- Use ellipses (...) for hesitation or trailing thought.
- Add at most one tag per clause; tag sparingly.
- Preserve every original word in the original order.
- Match the tone hint if one is provided.
- Use [break <duration>] between list items when the passage contains
  a bulleted or numbered list.

DO NOT:
- Rewrite, paraphrase, add, or remove any words from the passage.
- Add tags inside quoted speech that already conveys emotion.
- Stack multiple emotion tags consecutively.
- Output anything except the annotated passage. No preamble, no
  explanation, no markdown fences.

LEGAL TAGS:
[laughs] [chuckles] [sighs] [whispers] [excited] [curious] [serious]
[sarcastic] [thoughtful] [warm] [hesitant] [emphasis] [break 0.3s]
[break 0.5s] [break 1s]

PASSAGE:
{text}

ANNOTATED PASSAGE:
```

(Final prompt text lives in `parley-core::tts::annotator::prompt` and may be tuned via listening tests; spec captures the shape and constraints, not the exact wording.)

For `ProviderKind::ElevenLabsV3` and `OtherTagAware`, the prompt asks for square-bracket audio tags. For other tag-aware providers with different conventions (e.g., SSML), a separate `Annotator` implementation can target that syntax. v1 ships the v3-style annotator only.

### 3.4 List-Item Pacing

When the chunk contains a list (per the chunker's `[paragraph + list]` grouping), the annotator inserts pacing markers between items:

- Tag-aware providers: `[break 0.3s]` (or whatever the prompt specifies).
- Plain text fallback (annotator disabled): the chunker emits the list with natural newlines; the synthesizer pauses naturally between items.

The annotator handles this through the system prompt directive ("Use [break <duration>] between list items"), not as a separate code path. This keeps the chunker free of provider-format coupling.

### 3.5 Concurrency Model

```rust
struct AnnotatorQueue {
    /// In-flight annotations, keyed by chunk index, in dispatch order.
    pending: VecDeque<(u32, JoinHandle<Result<String, AnnotatorError>>)>,
    /// Original chunks, parallel to `pending`, used for fallback on error.
    originals: VecDeque<(u32, ReleasedChunk)>,
    /// The next index to dispatch to TTS.
    next_index: u32,
}
```

- `enqueue(chunk)` spawns the annotation task, pushes to both deques.
- `try_dispatch()` peeks the front of `pending`; if its `JoinHandle` is ready, take the result, pair with the original, and return the chunk to dispatch. If not ready, return `None` and wait.
- On task error or annotator failure: log a warning, dispatch the *original* chunk text.

The orchestrator's per-turn task loop alternates between feeding new chunks into the queue, awaiting completion of the front annotation, and dispatching the result to single-flight TTS.

### 3.6 Failure Modes

| Failure | Behavior |
|---|---|
| Haiku call returns error | Log warning with chunk index and error. Dispatch original text. |
| Haiku call times out (configurable, default 5s) | Treated as error; fall back to original. |
| Haiku returns text that doesn't preserve all original words | Logged at warn level. Use the annotator output anyway — degraded prosody is preferable to a hard error. (A future enhancement could enforce word-preservation by diffing and rejecting.) |
| Provider doesn't support tags | Annotator never runs; chunk dispatched unchanged. |
| Annotator returns empty string | Treated as error; fall back to original. |

### 3.7 Configuration

Configuration lives alongside the chunking config in `ModelConfig`:

```toml
[tts_annotator]
enabled = true            # Master switch; respects provider capability regardless
model = "claude-haiku-4"  # LLM model id used by HaikuAnnotator
timeout_ms = 5000
tone_hint = ""            # Optional default persona tone; empty = none
```

Per-persona annotator override (different prompt, different model, or disabled per persona) is **future work** and not part of v1's `Persona` schema.

## 4. Data Model

In `parley-core::tts::annotator`:

```rust
pub trait Annotator: Send + Sync { /* §3.2 */ }
pub struct AnnotationContext { /* §3.2 */ }
pub enum ProviderKind { /* §3.2 */ }

pub struct HaikuAnnotator {
    llm_client: Arc<dyn LlmClient>, // existing proxy LLM client
    model_id: String,
    timeout: Duration,
}

impl HaikuAnnotator {
    pub fn new(llm_client: Arc<dyn LlmClient>, config: AnnotatorConfig) -> Self;
}

#[async_trait]
impl Annotator for HaikuAnnotator { /* ... */ }
```

A `NoopAnnotator` (returns input unchanged) is provided for tests and for the configuration `enabled = false` case.

### 4.1 Wire & Cache

No change. The annotated text is internal to the orchestrator pipeline. Only the synthesized audio bytes flow over the wire and into the cache.

### 4.2 Logging & Observability

Each annotator call logs at debug level: chunk index, input character count, output character count, latency, model used. Failures log at warn level with the error and the chunk index. No annotator output is logged at info level by default to keep logs clean.

## 5. Test Plan

| Component | Tests |
|---|---|
| `HaikuAnnotator` (with mock `LlmClient`) | (1) Simple paragraph in → annotated paragraph out (mock returns text with one tag injected). (2) Annotator output preserves all original words (assert by tokenized comparison). (3) Empty annotator output → returns `Err`. (4) Mock returns after timeout → returns `Err(Timeout)`. (5) Tone hint passed in `AnnotationContext` is forwarded into the system prompt (verify via mock's recorded request). (6) `ProviderKind::ElevenLabsV3` produces square-bracket tag syntax in the system prompt. |
| `AnnotatorQueue` | (1) Single chunk, annotation succeeds → dispatched with annotated text. (2) Single chunk, annotation fails → dispatched with original text. (3) Three chunks enqueued with annotations completing in reverse order (chunk 2 finishes first, then 0, then 1) → dispatched in 0,1,2 order. (4) Provider with no expression instruction → annotator never invoked, chunks dispatched unchanged. (5) `enabled = false` in config → annotator never invoked even when provider supports tags. |
| Orchestrator integration | (1) ElevenLabs v3 turn with mock annotator: text reaches TTS provider with annotation tags. (2) ElevenLabs v2 turn (no tag support): text reaches TTS provider unchanged. (3) v3 turn with annotator failing on chunk 1: chunk 0 annotated, chunk 1 falls back to original, chunk 2 annotated; all dispatched in order; warning logged for chunk 1. (4) v3 turn where TTS for chunk 0 takes 2s and annotator for chunk 1 takes 200ms: chunk 1 dispatch happens immediately when chunk 0's TTS completes (annotation already finished). |
| List pacing | (1) Chunk containing a `[paragraph + list]` block → annotated output contains `[break 0.3s]` markers between list items (verified via mock annotator that returns a stub annotation with breaks). (2) Same chunk with annotator disabled → reaches TTS with bare list newlines unchanged. |

## 6. Migration & Compatibility

- New code only. No changes to existing wire formats, cache layout, or session files.
- Default config (`enabled = true`) means existing v3 personas get expressive audio automatically once this lands. If listeners react badly, set `enabled = false` in `ModelConfig` and ship a hotfix while we tune the prompt.
- v2 personas and other tag-unaware providers are unaffected.

## 7. Open Questions

1. **Word-preservation enforcement.** Should we hard-reject annotator output that drops or adds words, or accept it with a warning? v1 accepts; tests document the divergence. Tighten if listening tests show drift.
2. **Per-persona tone hint plumbing.** The `tone_hint` field is in `AnnotationContext` but `Persona` has no `tone` field today. Future work in [persona CRUD](persona-crud-spec.md). For v1, `tone_hint` is always `None`.
3. **Annotator caching.** Same input text + same context could deterministically cache to the same annotated output. Skipped for v1 to avoid stale prompt risk; revisit if Haiku call cost becomes a real concern.
4. **Provider-specific tag dialects.** xAI's current legal tag subset and scoped-wrapper bridge are documented in [xAI TTS Prosody Improvement 1](xai-improve-1.md). Longer-term explicit span data is still the preferred model for non-zero-width expression ranges.

## 8. Out-of-Scope Reminders

- Per-persona annotator prompts (NG1).
- Annotation of user input (NG2).
- Voice-cloning or speaker-identity tags (NG3).
- Word-level timing alignment (NG4).
- Streaming annotator output (NG5).
- Chunker boundary modification (NG6).
