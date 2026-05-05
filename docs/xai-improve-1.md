# xAI TTS Prosody Improvement 1

**Status:** Active
**Type:** Design
**Audience:** Both
**Date:** 2026-05-04

## 1. Purpose

Improve xAI text-to-speech naturalness in Parley's conversation path without breaking provider portability.

The current xAI path speaks correctly but often feels wooden: intonation is mostly sentence-local, pauses are mechanical, and the delivery has little emotional contour. The first improvement should use capabilities xAI already exposes while preserving Parley's neutral expression vocabulary and provider-swappable architecture.

This document is the source of truth for the first xAI prosody slice. It narrows and updates the relevant parts of [xAI Speech Integration](xai-speech-integration-spec.md), [Expressive Annotation](expressive-annotation-spec.md), [Conversation Mode](conversation-mode-spec.md), and [Paragraph-Bounded TTS Chunking](paragraph-tts-chunking-spec.md).

## 2. Pre-Slice State

| Area | Behavior before this slice | Consequence |
| --- | --- | --- |
| xAI adapter | `proxy::tts::xai::XaiTts` uses unary `POST /v1/tts`. | Each chunk is synthesized as an independent request. |
| Expression support | xAI had no provider-specific `expression_tag_instruction()`. | The orchestrator did not teach the LLM an xAI-safe expression surface for xAI turns. |
| Tag translation | xAI used the default `TtsProvider::translate_expression_tags`, which stripped `{tag}` markers. | Any neutral expression tags that appeared were removed before synthesis. |
| Chunking | `ChunkPlanner` releases paragraph-bounded chunks with a first-chunk fast path. | Better than sentence-only dispatch, but still limited by provider-side utterance resets. |
| Continuity hints | The orchestrator sends `SynthesisContext::previous_text`; xAI ignores it today. | Later chunks have no provider-native context about prior delivery. |
| Span model | The architecture and specs define span-shaped concepts, but the live expression path is inline point tags. | xAI wrapping tags such as `<soft>...</soft>` and `<emphasis>...</emphasis>` do not map cleanly yet. |

The stale implementation assumption was the most actionable problem: current xAI public docs state that TTS text supports inline speech tags and wrapping style tags. The implementation behaved as if bracketed tags would be read literally.

## 3. xAI Native Expression Surface

As of the current xAI Voice REST documentation, `text` for `/v1/tts` supports:

| Native shape | Examples | Parley interpretation |
| --- | --- | --- |
| Inline event tags | `[pause]`, `[long-pause]`, `[laugh]`, `[chuckle]`, `[breath]`, `[inhale]`, `[exhale]`, `[sigh]` | Point events that can map from the current neutral tag stream. |
| Wrapping style tags | `<soft>...</soft>`, `<whisper>...</whisper>`, `<slow>...</slow>`, `<fast>...</fast>`, `<emphasis>...</emphasis>`, `<laugh-speak>...</laugh-speak>` | Span controls that need a scoped expression representation. |

Decision: Parley should not ask personas to emit raw xAI tags. Personas should continue emitting provider-neutral tags from `parley_core::expression`; xAI-specific syntax belongs inside `XaiTts::translate_expression_tags()`.

## 4. Span Infrastructure Answer

Gavin's intuition is right: Parley already has span-shaped infrastructure, but it is not currently connected to the live expression translator.

| Existing piece | Status | Relevance |
| --- | --- | --- |
| [Architecture annotated stream](architecture.md#core-data-model-the-annotated-stream) | Conceptual model: annotations are spans pinned to time ranges. | This is the long-term storage shape for audio-linked expression. |
| [Word Graph](word-graph-spec.md) and `parley-core::word_graph` | Implemented minimal slice: nodes carry `start_ms` / `end_ms`; deferred editing/projection span ops remain out of scope. | Good foundation for time spans, but not a TTS expression-input API yet. |
| [Conversation Mode TTS sketch](conversation-mode-spec.md#124-tts-provider-trait) | Spec sketch includes `TtsInput { text, annotations: Vec<ExpressionSpan> }`. | This is the closest existing design for expression spans. It is not the implemented `TtsProvider` trait today. |
| `parley_core::expression` | Implemented neutral vocabulary and parser for inline `{tag}` markers. | Current production path. It models tag points, not scoped spans. |

Conclusion: for this first slice, implement xAI point-tag support using the existing inline neutral tags. Do not retrofit the full `TtsProvider` trait to `ExpressionSpan` yet. Instead, document and prepare a second slice that upgrades neutral expressions from point markers to scoped spans.

## 5. Scope

### In Scope

- Enable xAI as an expressive-tag-capable provider.
- Translate safe point-shaped neutral tags into xAI native inline event tags.
- Tune xAI REST chunking toward paragraph-shaped requests because the REST endpoint has no provider-side continuation field.
- Add unit tests for xAI tag capability and tag translation.
- Update stale comments/docs that say xAI cannot render expressive tags.
- Preserve provider neutrality: persona prompts use `{warm}`, `{pause:short}`, etc., never raw `[pause]` or `<soft>` xAI syntax.
- Define the span follow-up clearly enough that implementation can happen without relitigating the model.

### Out of Scope

- Full xAI WebSocket TTS adapter.
- Replacing the current `TtsProvider` trait with `TtsInput` / `ExpressionSpan`.
- A Haiku-backed post-LLM annotator pass.
- Raw xAI tags in persona files or user-authored prompts.
- Voice cloning, custom voice creation, or changing the default xAI voice.

### Deferred

- Scoped expression spans for wrapping tags.
- Long-lived xAI WebSocket per turn or per session.
- Undocumented xAI request fields for previous/next text context.
- Listening-test harness and saved A/B audio artifacts.
- Rich provider expression capability data beyond `expression_tag_instruction()` for future span-aware renderers, as anticipated by [Hume Octave 2 TTS Integration §10](hume-octave-2-tts-integration-spec.md#10-expression-strategy).

## 6. Requirements

| ID | Requirement | Verification |
| --- | --- | --- |
| XAI-PROS-01 | `XaiTts::expression_tag_instruction()` returns xAI-specific prompt text. | xAI provider unit test. |
| XAI-PROS-02 | The orchestrator prepends the active provider's `TtsProvider::expression_tag_instruction()` when the persona permits expression annotations. | Focused orchestrator regression test proving provider-owned instruction text is used. |
| XAI-PROS-03 | xAI translation maps `{laugh}` to `[laugh]`. | xAI provider unit test. |
| XAI-PROS-04 | xAI translation maps `{sigh}` to `[sigh]`. | xAI provider unit test. |
| XAI-PROS-05 | xAI translation maps `{pause:short}` to `[pause]`. | xAI provider unit test. |
| XAI-PROS-06 | xAI translation maps `{pause:medium}` and `{pause:long}` to `[long-pause]` for the first slice. | xAI provider unit test; revisit after listening tests. |
| XAI-PROS-07 | xAI advertises only the expression tags its translator can render safely. | xAI provider unit test for `expression_tag_instruction()`. |
| XAI-PROS-07a | xAI style tags `{soft}`, `{thoughtful}`, `{emphasis}`, and `{excited}` render as scoped native wrappers over the following text unit. | xAI provider unit tests. |
| XAI-PROS-07b | Unsupported style labels such as `{warm}`, `{empathetic}`, and `{sarcastic}` are stripped rather than rendered as literal text. | xAI provider unit test. |
| XAI-PROS-08 | Translation preserves all literal words and punctuation in order. | xAI provider unit test using mixed text and tags. |
| XAI-PROS-09 | Existing non-xAI providers keep their current expression behavior. | Existing ElevenLabs / Cartesia / orchestrator tests continue to pass. |
| XAI-PROS-10 | `XaiTts::tune_chunk_policy()` disables the eager first-chunk sentence-count split and aligns idle timeout with paragraph wait. | xAI provider unit test. |
| XAI-PROS-11 | The orchestrator uses the active TTS provider's tuned chunk policy before constructing `ChunkPlanner`. | Orchestrator regression test proving a tuned provider keeps a three-sentence first paragraph in one TTS request. |

## 7. Initial Translation Table

| Neutral tag | xAI output | Rationale |
| --- | --- | --- |
| `{laugh}` | `[laugh]` | Direct native event. |
| `{sigh}` | `[sigh]` | Direct native event. |
| `{pause:short}` | `[pause]` | Closest documented short pause control. |
| `{pause:medium}` | `[long-pause]` | xAI only exposes coarse pause tags; choose audible separation. |
| `{pause:long}` | `[long-pause]` | Same first-slice mapping; duration refinement waits on listening tests. |
| `{soft}` | `<soft>...</soft>` around the following sentence/clause | Direct native style wrapper; prompted as scoped to the following text unit. |
| `{thoughtful}` | `<slow>...</slow>` around the following sentence/clause | Best available xAI-native proxy for slower considered delivery. |
| `{emphasis}` | `<emphasis>...</emphasis>` around the following word/phrase or sentence/clause | Direct native style wrapper. |
| `{excited}` | `<fast>...</fast>` around the following sentence/clause | Conservative proxy for animated delivery; revisit with listening tests. |
| `{warm}` / `{empathetic}` / other emotion cues | strip | No close native control yet; do not pretend these map cleanly. |

This is still conservative. Bad expressive markup is worse than no markup because it can produce audible artifacts or literal tag speech. The key change from the first production smoke test is that xAI now owns a narrower provider-specific prompt, so it does not ask the LLM for tags that its translator cannot render.

## 8. Chunk Continuity Strategy

xAI's REST `/v1/tts` path has no documented equivalent of ElevenLabs `previous_text` or Cartesia `context_id`. Sending prior text in the request body would either be ignored or risk validation failure; putting prior text into `text` would make xAI read it aloud. Therefore the first continuity improvement stays inside Parley's chunk planner.

`XaiTts::tune_chunk_policy()` adjusts the model's configured [`ChunkPolicy`](paragraph-tts-chunking-spec.md#34-configuration):

- `first_chunk_max_sentences = 0`, disabling the universal first-chunk fast path that otherwise cuts after two sentences.
- `idle_timeout_ms = max(idle_timeout_ms, paragraph_wait_ms)`, preventing normal token-stream pauses from beating the paragraph wait window.

Effect: when the LLM emits a complete paragraph quickly, xAI receives that paragraph as one synthesis request instead of two smaller requests. That gives xAI more local context for intonation and reduces audible request resets. When no paragraph break arrives, the existing paragraph wait, hard cap, and stream-end rules still bound latency and request size.

This is provider-specific on purpose. Providers with native continuation can keep the lower-latency default policy; xAI REST trades some time-to-first-audio for fewer chunk boundaries.

## 9. Span Follow-Up Design

The correct abstraction for xAI wrapping tags is a scoped expression annotation:

```rust
// Rough sketch — not a specification.
pub struct ExpressionSpan {
    pub tag: ExpressionTag,
    pub start_char: usize,
    pub end_char: usize,
}
```

The old [Conversation Mode TTS provider sketch](conversation-mode-spec.md#124-tts-provider-trait) already points in this direction. A future expression slice should turn that sketch into an implemented API with these constraints:

- Spans are over clean rendered text, not over provider-native tag-expanded text.
- `start_char` and `end_char` are UTF-8 byte offsets aligned to char boundaries.
- Zero-width spans represent events such as laugh, sigh, and pause.
- Non-zero spans represent style controls such as soft, slow, fast, and emphasis.
- Provider translators are pure functions from `(text, spans)` to provider-native text.
- Session persistence stores neutral spans, not provider-native tags, so historical AI turns can be re-synthesized by another provider.

### Proposed Span Translation Examples

| Neutral span | xAI native rendering |
| --- | --- |
| `pause:short` at offset 12, zero width | Insert `[pause]` at offset 12. |
| `emphasis` over `really matters` | `<emphasis>really matters</emphasis>` |
| `soft` over `I hear you` | `<soft>I hear you</soft>` |
| `thoughtful` over a clause | Potentially `<slow>...</slow>` after listening tests prove it helps. |

## 10. Implementation Plan

1. Implement `XaiTts::expression_tag_instruction()` with the xAI-supported expression surface.
2. Implement `XaiTts::translate_expression_tags()` using `parley_core::expression::split_into_segments()`.
3. Add xAI provider tests for the requirements in §6.
4. Update stale xAI comments and docs that say xAI reads expressive tags literally.
5. Implement `TtsProvider::tune_chunk_policy()` and route planner construction through the active provider.
6. Tune xAI REST chunking for paragraph continuity.
7. Manually listen to fixed responses through xAI with and without neutral expression tags and with multi-paragraph output.
8. If scoped delivery remains brittle, write the explicit expression-span slice before widening the style vocabulary.

## 11. Listening Test Script

Use the same semantic content for every run:

```text
That's a good question. Let me think about it for a second. I don't want to overstate the answer, but there is a real difference here. The first option is safer. The second option is faster. If we're optimizing for trust, I would choose the safer one.
```

Suggested tagged variant:

```text
That's a good question. {pause:short} Let me think about it for a second. {sigh} I don't want to overstate the answer, but there is a real difference here. The first option is safer. {pause:short} The second option is faster. If we're optimizing for trust, I would choose the safer one.
```

Listening criteria:

- Does xAI render `[pause]` and `[sigh]` naturally, or does it overact?
- Do pauses improve comprehension without making the agent sound slow?
- Does a three-sentence paragraph stay in one request in the proxy logs/tests?
- Does paragraph-shaped chunking reduce the reset effect at sentence and paragraph boundaries?
- Does any tag ever leak as spoken literal text?

## 12. Open Questions

| ID | Question | Recommendation |
| --- | --- | --- |
| OQ-01 | Should `{pause:medium}` and `{pause:long}` both map to `[long-pause]`? | Yes for first slice; refine only with listening evidence. |
| OQ-02 | Should `{emphasis}` wrap the next word/clause heuristically before spans exist? | Yes for this listening slice, but scope it to the following punctuation-delimited text unit and keep explicit spans as the long-term fix. |
| OQ-03 | Should xAI WebSocket TTS land in the same PR as point tags? | No. It is valuable, but it changes transport and failure behavior. |
| OQ-04 | Should the post-LLM annotator run for xAI now? | No. First prove direct neutral tags generated by the response LLM are useful. |
| OQ-05 | Should xAI REST use undocumented previous/next text fields for continuity? | No. Use provider-tuned chunking until xAI documents a continuation field or we implement a supported streaming/session path. |

## 13. Decision Record

Decision: ship provider-specific expression translation plus xAI REST chunk-policy tuning. xAI can use native point tags and a conservative punctuation-scoped wrapper bridge; deeper continuity comes from paragraph-shaped requests until a supported xAI session/streaming path exists.

Alternatives considered:

- Enable raw xAI tags in persona prompts. Rejected because it breaks provider portability.
- Heuristically wrap the next word or clause for style tags. Accepted narrowly for `{soft}`, `{thoughtful}`, `{emphasis}`, and `{excited}` so listening tests can verify whether native xAI wrappers help before the span model lands.
- Build the full span-based TTS input API before any xAI improvement. Rejected because it delays a small, testable quality improvement.
- Implement xAI WebSocket first. Deferred because transport changes have broader blast radius than tag translation.
- Invent previous/next-text fields for xAI REST. Rejected because unsupported provider fields are brittle and could fail requests.

Tradeoff accepted: xAI REST may have a slightly slower first audio response because it waits for a paragraph boundary more often. That is acceptable for this slice because the user's current pain is delivery continuity, not absolute minimum TTFA.
