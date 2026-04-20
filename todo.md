# Parley — TODO

## Phase 0: Project Scaffold

- [ ] Initialize Cargo workspace and Dioxus project
- [ ] Set up module structure (models, providers, audio, engine, persistence, ui)
- [ ] Define core data types (Session, Transcript, Speaker, Prompt, Profile, Pipeline, Vocabulary)
- [ ] Add key dependencies to Cargo.toml (dioxus, cpal, symphonia, serde, toml, tokio-tungstenite)
- [ ] Create Dioxus.toml config
- [ ] Verify the app compiles and launches an empty window

## Phase 1: Audio Capture & Encoding

- [ ] Enumerate audio input devices via cpal
- [ ] Capture microphone audio to an in-memory PCM buffer
- [ ] Multi-channel capture — preserve all channels the device offers
- [ ] Mono mixdown utility for STT pipeline
- [ ] FLAC encoding (lossless storage)
- [ ] Opus encoding with configurable bitrate and application mode
- [ ] Audio file import via symphonia (MP3, M4A, WAV, FLAC, OGG)
- [ ] Basic record/stop controls in the UI

## Phase 2: Streaming STT

- [ ] Define `SttProvider` trait (stream + process_file)
- [ ] Implement Deepgram streaming provider (WebSocket, `punctuate=true`, `smart_format=true`)
- [ ] Wire audio capture → mono mix → WebSocket stream
- [ ] Handle interim results (live-as-you-speak text) and final results
- [ ] Configurable endpointing (long value or disabled for continuous listening)
- [ ] Display live transcript in a scrolling Dioxus component

## Phase 3: Persistence

- [ ] Design `~/.parley/` directory layout and create on first run
- [ ] TOML config loading/saving (global config, profiles)
- [ ] Markdown + YAML frontmatter serialization for transcripts
- [ ] Markdown prompt file loading
- [ ] Session storage: save transcript .md + .timing.json + audio file together
- [ ] Session listing: browse past sessions by date
- [ ] Vocabulary TOML loading

## Phase 4: Profiles & Prompts

- [ ] Profile CRUD — create, edit, switch between profiles
- [ ] Audio format settings per profile (FLAC vs Opus, bitrate, channels, sample rate)
- [ ] Prompt editor in UI — create and edit prompt markdown files
- [ ] Profile selector in recording controls
- [ ] Prompt selector in recording controls

## Phase 5: Post-Processing Pipeline

- [ ] Define `LlmProvider` trait
- [ ] Implement OpenAI/Claude provider for post-processing
- [ ] Configurable pipeline stages per profile
- [ ] Post-processing pass: clean transcript through LLM with the selected prompt
- [ ] Confidence flagging: mark low-confidence words inline in transcript
- [ ] Vocabulary-aware flagging: flag words not in custom vocabulary

## Phase 6: Diarization & Speaker Identification

- [ ] Enable provider-level diarization (Deepgram `diarize=true`)
- [ ] Render speaker labels in transcript (Speaker_0, Speaker_1, ...)
- [ ] LLM contextual speaker identification pass
- [ ] Speaker mapping storage in session frontmatter
- [ ] Render identified names in transcript
- [ ] Multi-channel-aware diarization (feed channel separation as a signal)

## Phase 7: File Processing Mode

- [ ] Batch processing: select an audio file → decode → STT → transcript
- [ ] Progress indicator for file processing
- [ ] Same pipeline as live recording (diarization, post-processing, confidence flagging)
- [ ] Drag-and-drop file import

## Phase 8: Conversation Mode (v1)

> Full specification: `docs/conversation-mode-spec.md`

- [ ] Define `LlmProvider` shared trait + provider-specific extension traits (Anthropic, OpenAI)
- [ ] Define `TtsProvider` trait (returning audio + word-level timings)
- [ ] Implement ElevenLabs TTS provider with streaming synthesis
- [ ] Persona schema + storage (`~/.parley/personas/*.toml`)
- [ ] Model config schema + storage (`~/.parley/models/*.toml`)
- [ ] Conversation Mode toggle (top-level Capture ↔ Conversation)
- [ ] Conversation Orchestrator — turn state machine, history, persona resolution, dispatch
- [ ] Sentence-boundary chunking with single-token lookahead for TTS streaming
- [ ] Conversation UI — interleaved user/AI transcript with per-turn cost & Play button
- [ ] Per-turn TTS audio cache (`tts-cache/turn-NNN.opus` in session dir)
- [ ] Pause / Stop / Play controls (per §5.3 of spec)
- [ ] Press-to-start / press-to-end turn-taking
- [ ] Multi-party self-introduction protocol (phonetic-rich sentence, per-speaker buttons)
- [ ] Real-time diarization integration with manual speaker-tagging fallback
- [ ] Crude VAD barge-in with pending-input capture
- [ ] Token-based context compaction with word-count fallback
- [ ] Summary turns (`Speaker.kind = system`, `was_compacted_from = [turn_ids]`)
- [ ] Context utilization indicator in UI
- [ ] Per-turn + running-total cost tracking (extends existing meter)
- [ ] Failure handling: meta-turn announcing provider errors with Retry/Skip
- [ ] Session frontmatter additions (`mode`, `personas_used`, `compaction_events`)

## Phase 9: Conversation Mode v1.5 — Polish & Diarization Confidence

- [ ] Cross-session voice fingerprinting persistence
- [ ] Diarization quality validation; hide manual fallback by default if quality is good
- [ ] Per-persona vocabulary integration with existing vocabulary system
- [ ] TTS playback speed adjustment

## Phase 10: Conversation Mode v2 — Multi-Tier Orchestration

- [ ] Two agents per persona (fast + heavy) with handoff state in turn state machine
- [ ] Fast-model acknowledgement before heavy-model response
- [ ] Distinct voices per tier
- [ ] Re-hydration of compacted content into live context
- [ ] Mid-session persona/model switching UI affordance
- [ ] Tools / function calling support

## Phase 11: Conversation Mode v3 — Expensive Narration & Full-Duplex

- [ ] Live narration of heavy-model reasoning streams
- [ ] True barge-in (audio-native via Gradium-style provider)
- [ ] Cross-session memory
- [ ] Reader-style word-level highlighting and click-to-play
- [ ] Provider failover (LLM A errors → automatic fallback to LLM B)
- [ ] Document reading: parse markdown files and synthesize speech via TTS

## Phase 12: Local/Offline Mode

- [ ] Local Whisper STT via ONNX Runtime (`ort` crate)
- [ ] Fallback logic: try cloud provider, fall back to local on failure
- [ ] Settings toggle for "offline only" mode

## Phase 13: Voice Fingerprinting (Future)

- [ ] Speaker embedding model integration (ECAPA-TDNN via ONNX)
- [ ] Voice embedding storage across sessions
- [ ] Cross-session speaker recognition
- [ ] Speaker management UI (name, merge, delete voice profiles)

## Phase 14: Polish & Gossamer Integration

- [ ] Extract `parley-core` library crate (everything except `ui/`)
- [ ] CLI interface for headless operation
- [ ] Gossamer integration planning
- [ ] Keyboard shortcuts (global hotkey for record/stop)
- [ ] System tray / background operation
- [ ] Copy-to-clipboard, send-to-agent shortcuts
