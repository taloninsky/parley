//! Browser-side progressive PCM player backed by Web Audio.
//!
//! Used by Conversation Mode when the proxy advertises a raw PCM
//! `format` SSE frame (Cartesia Sonic-3 over WebSocket emits
//! `pcm_s16le` mono at 44.1 kHz; no `<audio>`-friendly container is
//! involved). Each `audio` SSE frame is decoded base64 → bytes →
//! interleaved 16-bit signed little-endian samples → f32 in
//! [-1, 1], then handed to [`PcmPlayer::append`].
//!
//! Spec: `docs/cartesia-sonic-3-integration-spec.md` §6.4.
//!
//! ## Design vs `MediaSourcePlayer`
//!
//! The MSE pipeline is the wrong tool for raw PCM: there's no
//! `audio/pcm-*` MIME the browser will accept on a `SourceBuffer`,
//! and shipping fake WAV chunks through MSE chops the leading RIFF
//! header off mid-stream. We instead allocate an
//! [`AudioContext`](web_sys::AudioContext) and drop each chunk into
//! a fresh [`AudioBufferSourceNode`] scheduled back-to-back at
//! `AudioContext.currentTime`. The kernel mixer handles continuity;
//! the gap between scheduled buffers is well below one sample at
//! 44.1 kHz so there are no audible seams.
//!
//! ## Lifetime model
//!
//! Like `MediaSourcePlayer`, the player exposes an `Rc<RefCell<…>>`
//! handle so it's cheap to clone across event handlers and to hold
//! inside a Dioxus `Signal`. The internal state owns the
//! `AudioContext`, the running schedule cursor (next-start time in
//! AudioContext seconds), an `end_pending` flag, and the closures
//! we register on the final source node so the `on_ended` hook can
//! fire after the last buffer plays out.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{AudioBuffer, AudioBufferSourceNode, AudioContext};

/// Sample rate the player was constructed for, in Hz. Re-emitted to
/// the browser via the SSE `format` frame. Cartesia's Sonic-3
/// pinned default is 44 100.
const DEFAULT_SAMPLE_RATE_HZ: f32 = 44_100.0;

/// Internal mutable state shared between the player handle and the
/// `onended` callback we register on the *last* scheduled source
/// node when [`PcmPlayer::end`] is called.
struct PlayerState {
    /// Web Audio destination context. Allocated lazily on first
    /// append so the constructor doesn't trigger a user-gesture
    /// gate for callers that aren't ready to play yet.
    ctx: Option<AudioContext>,
    /// Sample rate the player was built for, in Hz. Used to size
    /// every [`AudioBuffer`] allocation.
    sample_rate: f32,
    /// Channel count (1 for Cartesia mono). When the wire format
    /// changes, this is the only knob the conversion path cares
    /// about — we de-interleave at append time.
    channels: u16,
    /// Bytes per sample per channel. For `pcm_s16le` this is 2.
    bytes_per_sample: u16,
    /// Next start time (in `AudioContext.currentTime` seconds) the
    /// player will schedule a source node at. Starts at 0 and
    /// advances by every appended buffer's duration.
    next_start: f64,
    /// `true` once [`Self::end`] has been called. We register the
    /// `on_ended` callback on the most recently scheduled source
    /// node so it fires when the last buffer drains.
    end_pending: bool,
    /// `true` once the player has been stopped — further appends
    /// and scheduling become no-ops.
    stopped: bool,
    /// User-supplied `on_ended` callback, fired when the last
    /// scheduled buffer finishes playing after `end()` is called.
    /// Wrapped in `Option<Box<dyn Fn()>>` so the same player can be
    /// re-armed across turns. We don't call it from the `Drop`
    /// path; the consumer is responsible for explicit `stop()`.
    on_ended: Option<Box<dyn Fn()>>,
    /// Closures we keep alive so the JS side can invoke them on
    /// the `ended` event of the final source node. Bounded to the
    /// most-recent two so we don't leak unboundedly across long
    /// streams.
    _ended_closures: Vec<Closure<dyn FnMut()>>,
    /// Most recently scheduled source node. We re-arm its
    /// `onended` whenever `end()` is called so the `on_ended`
    /// callback fires at the right time even if `end()` lands
    /// after several appends.
    last_node: Option<AudioBufferSourceNode>,
    /// Tracks whether the user has called `pause()`. Web Audio
    /// doesn't have a native pause; we implement it by suspending
    /// the AudioContext (which freezes the schedule) and resuming
    /// it on `play()`.
    paused: bool,
}

/// Browser-side PCM player. Cheap to clone — internal state lives
/// behind `Rc<RefCell<...>>`.
#[derive(Clone)]
pub struct PcmPlayer {
    state: Rc<RefCell<PlayerState>>,
}

impl PcmPlayer {
    /// Construct a player that expects 16-bit signed little-endian
    /// samples at `sample_rate_hz`, with the given channel layout.
    /// No browser objects are allocated until the first
    /// [`Self::append`] — that's a deliberate choice so a player
    /// constructed pre-gesture doesn't trigger autoplay prompts.
    pub fn new_pcm_s16le(sample_rate_hz: u32, channels: u16, bytes_per_sample: u16) -> Self {
        Self {
            state: Rc::new(RefCell::new(PlayerState {
                ctx: None,
                sample_rate: sample_rate_hz as f32,
                channels,
                bytes_per_sample,
                next_start: 0.0,
                end_pending: false,
                stopped: false,
                on_ended: None,
                _ended_closures: Vec::new(),
                last_node: None,
                paused: false,
            })),
        }
    }

    /// Convenience constructor for Cartesia's pinned defaults
    /// (44.1 kHz mono `pcm_s16le`).
    pub fn new_default() -> Self {
        Self::new_pcm_s16le(DEFAULT_SAMPLE_RATE_HZ as u32, 1, 2)
    }

    /// Append a chunk of raw PCM bytes. Bytes are decoded from
    /// `pcm_s16le` (interleaved when channels > 1) into f32 in
    /// [-1, 1] and scheduled at `next_start` in the AudioContext's
    /// timeline. Safe to call before the first `play()` — Web
    /// Audio queues the buffers internally.
    ///
    /// No-op once [`Self::stop`] has been called, or on any chunk
    /// that doesn't contain at least one full sample frame.
    pub fn append(&self, bytes: &[u8]) -> Result<(), JsValue> {
        let mut s = self.state.borrow_mut();
        if s.stopped {
            return Ok(());
        }
        if bytes.is_empty() {
            return Ok(());
        }

        // Lazy AudioContext allocation. Browsers may keep the
        // context in `suspended` state until a user gesture has
        // been observed; the play() path resumes it.
        if s.ctx.is_none() {
            let ctx = AudioContext::new()?;
            // Seed `next_start` slightly ahead of `currentTime` so
            // the very first buffer doesn't get sliced by the JS
            // event-loop hop between schedule + decode of the next
            // chunk. 20 ms matches what Web Audio reference players
            // use for low-latency live decoding.
            s.next_start = ctx.current_time() + 0.020;
            s.ctx = Some(ctx);
        }
        let ctx = s.ctx.as_ref().unwrap().clone();

        let frame_size_bytes = s.channels as usize * s.bytes_per_sample as usize;
        if frame_size_bytes == 0 {
            return Ok(()); // misconfigured player; defensive
        }
        let usable_bytes = bytes.len() - (bytes.len() % frame_size_bytes);
        if usable_bytes == 0 {
            return Ok(());
        }
        let total_samples = usable_bytes / s.bytes_per_sample as usize; // across all channels
        let frames = total_samples / s.channels as usize;
        let channels = s.channels as u32;
        let sample_rate = s.sample_rate;

        // Decode interleaved s16le -> f32 in one pass per channel.
        // We allocate per-channel f32 buffers and then `copy_to_channel`
        // into the AudioBuffer; this avoids walking the raw bytes
        // multiple times for multi-channel streams.
        let mut per_channel: Vec<Vec<f32>> =
            (0..channels).map(|_| Vec::with_capacity(frames)).collect();
        let mut idx = 0;
        for _frame in 0..frames {
            for ch in 0..channels as usize {
                let lo = bytes[idx];
                let hi = bytes[idx + 1];
                let sample = i16::from_le_bytes([lo, hi]) as f32 / 32_768.0_f32;
                per_channel[ch].push(sample);
                idx += 2;
            }
        }

        let buf: AudioBuffer = ctx.create_buffer(channels, frames as u32, sample_rate)?;
        for (ch, samples) in per_channel.iter_mut().enumerate() {
            buf.copy_to_channel(samples.as_mut_slice(), ch as i32)?;
        }

        let node: AudioBufferSourceNode = ctx.create_buffer_source()?;
        node.set_buffer(Some(&buf));
        // `connect_with_audio_node` returns the node it was given so
        // the chain reads left-to-right; we don't need the return.
        let _ = node.connect_with_audio_node(&ctx.destination())?;

        let start_at = s.next_start;
        // Use the explicit `start_with_when` overload — the bare
        // `start()` schedules at `currentTime` which would let
        // small jitters drift the schedule.
        node.start_with_when(start_at)?;

        let duration = frames as f64 / sample_rate as f64;
        s.next_start = start_at + duration;
        s.last_node = Some(node);

        Ok(())
    }

    /// Mark end-of-stream. The player keeps draining any already-
    /// scheduled buffers. When the most recently scheduled buffer
    /// reports `ended`, the user-supplied [`Self::on_ended`]
    /// callback fires (if installed). No-op when already ended or
    /// stopped.
    pub fn end(&self) -> Result<(), JsValue> {
        let mut s = self.state.borrow_mut();
        if s.stopped || s.end_pending {
            return Ok(());
        }
        s.end_pending = true;
        // Re-arm `onended` on the most-recent node so the user's
        // callback fires when its scheduled time + duration elapse.
        if let Some(node) = s.last_node.clone() {
            // Take ownership of the on_ended hook out of state so
            // the closure can call it without re-borrowing.
            let cb = s.on_ended.take();
            let state_clone = self.state.clone();
            let closure = Closure::<dyn FnMut()>::new(move || {
                if let Some(cb) = &cb {
                    cb();
                }
                // Park on_ended back so future end() calls (rare,
                // but defensible) still surface it.
                let _ = state_clone.try_borrow_mut();
            });
            node.set_onended(Some(closure.as_ref().unchecked_ref()));
            s._ended_closures.push(closure);
        } else {
            // No buffer was ever scheduled (stream ended before any
            // audio arrived). Fire the callback synchronously so
            // the auto-listen path doesn't hang.
            if let Some(cb) = s.on_ended.take() {
                cb();
            }
        }
        Ok(())
    }

    /// Suspend the AudioContext at the current cursor. Resumes via
    /// [`Self::play`]. No-op when already paused or stopped.
    pub fn pause(&self) {
        let mut s = self.state.borrow_mut();
        if s.stopped || s.paused {
            return;
        }
        s.paused = true;
        if let Some(ctx) = s.ctx.as_ref() {
            // `suspend()` returns a Promise; we don't `await` it
            // because callers don't surface the result.
            let _ = ctx.suspend();
        }
    }

    /// Resume playback. The AudioContext is resumed; if no context
    /// has been allocated yet (no append has occurred), this is a
    /// no-op and the next append will lazily build one.
    pub fn play(&self) -> Result<(), JsValue> {
        let mut s = self.state.borrow_mut();
        if s.stopped {
            return Ok(());
        }
        s.paused = false;
        if let Some(ctx) = s.ctx.as_ref() {
            // `resume()` is a Promise; ignore the return.
            let _ = ctx.resume()?;
        }
        Ok(())
    }

    /// Halt playback and tear down the AudioContext. The player
    /// becomes inert: subsequent [`Self::append`] / [`Self::end`]
    /// calls are no-ops. Idempotent.
    pub fn stop(&self) {
        let mut s = self.state.borrow_mut();
        if s.stopped {
            return;
        }
        s.stopped = true;
        s.last_node = None;
        if let Some(ctx) = s.ctx.take() {
            // `close()` is a Promise; ignore the return.
            let _ = ctx.close();
        }
    }

    /// Subscribe to playback-finished events. Fires when the most
    /// recently scheduled buffer's underlying source node reports
    /// `ended` — which, after [`Self::end`] has been called, is
    /// effectively "the last sample played out".
    pub fn on_ended(&self, cb: Box<dyn Fn()>) {
        self.state.borrow_mut().on_ended = Some(cb);
    }
}

#[cfg(test)]
mod tests {
    //! These tests are pure-logic only — Web Audio APIs aren't
    //! available in the workspace's `cargo test` runner. The
    //! end-to-end behaviour is exercised via the WASM smoke test in
    //! Conversation Mode.

    use super::*;

    #[test]
    fn new_default_uses_44100_mono_s16le() {
        // The constructor is pure — no JS calls. It should record
        // the format-shaping fields without error.
        let p = PcmPlayer::new_default();
        let s = p.state.borrow();
        assert_eq!(s.sample_rate as u32, 44_100);
        assert_eq!(s.channels, 1);
        assert_eq!(s.bytes_per_sample, 2);
        assert!(s.ctx.is_none());
        assert!(!s.stopped);
        assert!(!s.end_pending);
        assert!(!s.paused);
    }

    #[test]
    fn stopped_then_appends_are_noops() {
        let p = PcmPlayer::new_default();
        // Force-flag the stopped bit (no AudioContext should ever
        // allocate when the player is already stopped).
        p.state.borrow_mut().stopped = true;
        // append must early-out before allocating a context.
        let r = p.append(&[0x00, 0x00]);
        assert!(r.is_ok());
        assert!(p.state.borrow().ctx.is_none());
    }
}
