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

## Phase 8: Full-Duplex & TTS

- [ ] Define `TtsProvider` trait
- [ ] Implement TTS provider (OpenAI, ElevenLabs, or local)
- [ ] Full-duplex mode: simultaneous capture + playback
- [ ] Conversation UI — interleaved user/AI transcript
- [ ] Document reading: parse markdown files and synthesize speech via TTS
- [ ] Voice model selection per profile

## Phase 9: Local/Offline Mode

- [ ] Local Whisper STT via ONNX Runtime (`ort` crate)
- [ ] Fallback logic: try cloud provider, fall back to local on failure
- [ ] Settings toggle for "offline only" mode

## Phase 10: Voice Fingerprinting (Future)

- [ ] Speaker embedding model integration (ECAPA-TDNN via ONNX)
- [ ] Voice embedding storage across sessions
- [ ] Cross-session speaker recognition
- [ ] Speaker management UI (name, merge, delete voice profiles)

## Phase 11: Polish & Gossamer Integration

- [ ] Extract `parley-core` library crate (everything except `ui/`)
- [ ] CLI interface for headless operation
- [ ] Gossamer integration planning
- [ ] Keyboard shortcuts (global hotkey for record/stop)
- [ ] System tray / background operation
- [ ] Copy-to-clipboard, send-to-agent shortcuts
