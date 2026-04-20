# Parley Philosophy

## What Parley Is

Parley is a bridge between human speech and machine understanding. It treats audio as a first-class input — not an afterthought bolted onto a text-based workflow. When you speak, Parley should make that speech *more useful* than if you had typed it.

## Guiding Principles

### 1. Speak First, Format Later

The act of speaking should never be interrupted by formatting concerns. Parley captures your words faithfully and then applies intelligence — punctuation, structure, cleanup — after the fact. The speaker's job is to think and talk. Parley's job is everything else.

### 2. Your Words, Your Way

Every person speaks differently. Every context demands different output. A meeting transcript is not a journal entry is not a code dictation session. Parley adapts to context through prompts and profiles, not by forcing a one-size-fits-all workflow. The user defines what "good output" means.

### 3. Audio Deserves Respect

Audio is more than words over time. It carries amplitude, frequency, phase, spatial position, and directionality — and all of that information is *meaningful*. Phase differences between microphone channels encode where a voice is coming from. Spatial separation tells you who is speaking, even when words overlap. Your brain does this instinctively — it steers attention across a noisy room using the phase delay between your two ears. Parley should aspire to the same.

Don't throw audio away, don't compress it carelessly, don't discard channels or phase relationships. Capture at the highest fidelity and channel count the situation allows. Store it alongside the transcript. A transcript without its audio is a summary — sometimes that's fine, but the choice should be deliberate. And when multi-channel audio is available, treat it as structured spatial data, not just redundant copies of the same signal.

### 4. Know Who's Talking

Most transcription tools treat speech as a stream of undifferentiated words. Humans don't. We care who said what. Parley should always be working toward identifying speakers — from dumb labels (Speaker 1, Speaker 2) to contextual identification (Sarah, Gavin) to voice recognition across sessions. This is table stakes that the industry has left on the floor.

### 5. Speech Is Parallel, Not Serial

Human conversation isn't turn-based. People interrupt, talk over each other, finish each other's sentences, make affirmative noises while someone else is speaking. Two people can say important things at the same time. Most transcription tools force this into a linear sequence and lose information in the process.

Parley's data model must accommodate concurrent speech streams — multiple lanes active simultaneously. Even when the current models aren't great at separating overlapping speech on a single channel, the architecture must not be the bottleneck. It should be ready for the models to catch up.

### 6. Models Come and Go

Today's best STT model is tomorrow's legacy. Parley does not marry a provider. Every external dependency — STT, TTS, LLM — sits behind a trait. Swapping Deepgram for AssemblyAI or a local Whisper model is a config change, not a rewrite. The core logic owns the pipeline; providers are pluggable.

### 7. Plain Files Are the Default, Not the Only Option

The default persistence is human-readable files: markdown, TOML, JSON sidecars. Everything is grep-able, version-controllable, and portable. If Parley disappears tomorrow, your data is still yours in formats any text editor can open.

But files are one *projection* of the in-memory model, not the model itself. When Parley folds into Gossamer, sessions might project into cells. In another context, they might go into a database. The core works with the annotated stream in memory and hands it to a storage trait. The trait decides the format. Don't couple the data model to a filesystem layout.

### 8. The Core Is the Product

The UI is a skin. The real value lives in the engine: the audio pipeline, the provider abstractions, the persistence layer, the processing logic. This core must be UI-independent so it can live inside Dioxus today, fold into Gossamer tomorrow, or run headless in a CLI. Never couple business logic to a rendering framework.

### 9. Don't Guess — Ask (But Know When to Shut Up)

When Parley encounters a word it doesn't recognize or a passage with low confidence, it should say so. Flag it inline. Surface it to the user. Over time, learn from corrections. A transcript that silently substitutes wrong words is worse than one that honestly marks its uncertainty.

But there's a tension: interrupting someone mid-thought to ask "did you mean mutex or mux?" can destroy the very flow of thought you're trying to capture. This is a trade-off space with three modes:

- **Off:** Just do your best. Don't flag anything. Maximize transcription fidelity without any interruption. Trust the model.
- **Defer:** Mark uncertain passages silently (inline flags, highlight, margin markers) but don't interrupt. The user reviews confidence issues after they're done speaking.
- **Immediate:** Interrupt and ask in real-time. Best for short dictation or when accuracy matters more than flow.

The right mode depends on the situation. A brain dump wants *off* or *defer*. Dictating a legal document wants *immediate*. This is a per-profile setting — part of the broader principle that Parley adapts to context, not the other way around.

### 10. Safety, Speed, and Strictness

Audio processing is latency-sensitive and runs alongside real-time UI updates, concurrent network streams, and file I/O. The implementation language must provide memory safety without a garbage collector, fearless concurrency, a strong type system that catches errors at compile time, and real-time capable performance. These are the non-negotiable properties — they're *why* we chose Rust, not the other way around. One language from the audio capture layer to the UI components. Minimize language boundaries — no JS bridges, no Python subprocesses, no FFI hairballs unless absolutely unavoidable.

### 11. Start Standalone, Merge Later

Parley begins as its own app to figure out what works. Iterate fast, learn what the right UX is for continuous dictation, meeting recording, file processing. Once the patterns stabilize, fold the core into Gossamer as a capability. Premature integration is premature optimization — figure out the product first.

### 12. Transcript and Audio Are Linked, Not Separate

A transcript is not a text file that happens to have been generated from audio. It is a *view* of the audio — every paragraph anchored to a time range, every word to a moment. Playback should feel like reading along: highlight the active paragraph, highlight the active word within it, let the user click any sentence and hear it. Think ElevenLabs Reader, not a PDF next to an MP3. The transcript and the audio are one thing shown two ways.

## What Parley Is

Parley is a flexible, configurable audio processing module. It can be a standalone desktop app, a CLI tool, a library embedded in a larger system, or a meeting bot — whatever the situation demands. It is not opinionated about *how* it gets used. It is opinionated about doing the processing well.

## What Parley Is Not

- **Not a note-taking app.** Parley produces transcripts and processed audio. What you do with them is your business.
- **Not, at its core, an AI assistant.** Parley's core is an audio-processing module. AI models are tools in its pipeline. Parley does, however, ship a **Conversation Mode** as an opt-in feature that uses the core to host turn-based exchanges with AI agents (see `docs/conversation-mode-spec.md`). Conversation Mode is a *consumer* of the core, not a redefinition of it. The audio core knows nothing about agents, personas, or turns — and that boundary is structural.
- **Not a closed system.** If you want to plug Parley into Zoom, Teams, a podcast workflow, or a custom bot — that's a valid use case, not a misuse. The core is a module; the surface area is yours to define.
