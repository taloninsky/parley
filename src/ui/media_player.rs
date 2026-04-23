//! Browser-side progressive MP3 player backed by `MediaSource`.
//!
//! Used by Conversation Mode to play TTS audio chunks as they
//! arrive from the proxy's `/conversation/tts/{turn_id}` SSE
//! stream. Each `audio` SSE frame is decoded and handed to
//! [`MediaSourcePlayer::append`]; the player queues chunks
//! internally until the underlying `SourceBuffer.updateend` fires,
//! then drains them in order.
//!
//! Spec: `docs/conversation-voice-slice-spec.md` §7.2.
//!
//! ## Lifetime model
//!
//! The player owns the `MediaSource`, a single `SourceBuffer` for
//! `audio/mpeg`, and the `<audio>` element it's attached to. The
//! audio element is appended to `document.body` (off-screen, no
//! controls) so the browser can drive playback without a visible
//! widget — UI controls live elsewhere and forward to
//! [`Self::play`] / [`Self::pause`] / [`Self::stop`].
//!
//! ## Concurrency
//!
//! The MSE spec only allows one `appendBuffer` call at a time per
//! `SourceBuffer` (a second call while `updating == true` throws
//! `InvalidStateError`). We serialize calls behind a per-player
//! `RefCell<VecDeque<Vec<u8>>>` and pump the queue from the
//! `updateend` callback. Same model is used to defer
//! `endOfStream()` until any in-flight appends complete.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{HtmlAudioElement, MediaSource, SourceBuffer};

/// MIME type the proxy hands us; ElevenLabs' `mp3_44100_128`
/// output is plain `audio/mpeg`. Browsers that support MSE all
/// accept this codec without a profile suffix.
const AUDIO_MIME: &str = "audio/mpeg";

/// Internal mutable state shared between the player handle and the
/// JS callbacks the player registers.
struct PlayerState {
    /// `SourceBuffer` becomes `Some` only after `MediaSource`
    /// transitions to `open` and `addSourceBuffer` returns. Until
    /// then incoming chunks queue up.
    source_buffer: Option<SourceBuffer>,
    /// FIFO of chunks waiting for the `SourceBuffer` to be ready
    /// (or to finish the previous append).
    queue: VecDeque<Vec<u8>>,
    /// `true` once the consumer has called [`MediaSourcePlayer::end`].
    /// We defer the actual `MediaSource.endOfStream()` call until
    /// the queue drains — calling it while `SourceBuffer.updating`
    /// is `true` throws.
    end_pending: bool,
    /// `true` once the player has been stopped — further appends
    /// and pumps become no-ops so a late SSE frame doesn't
    /// re-attach the audio element.
    stopped: bool,
    /// JS closures we keep alive for the lifetime of the player.
    /// `MediaSource`'s `sourceopen` and `SourceBuffer`'s
    /// `updateend` events fire from the browser, and the closures
    /// must outlive the call to `addEventListener`.
    _on_source_open: Option<Closure<dyn FnMut()>>,
    _on_update_end: Option<Closure<dyn FnMut()>>,
    _on_ended: Option<Closure<dyn FnMut()>>,
}

/// Browser-side progressive MP3 player. Cheap to clone — the
/// underlying state is `Rc<RefCell<...>>`.
#[derive(Clone)]
pub struct MediaSourcePlayer {
    media_source: MediaSource,
    audio: HtmlAudioElement,
    object_url: Rc<String>,
    state: Rc<RefCell<PlayerState>>,
}

impl MediaSourcePlayer {
    /// Construct a new player attached to a fresh, hidden
    /// `<audio>` element appended to `document.body`. The audio
    /// element is muted=`false` and autoplay-eligible; the browser
    /// may still gate playback until a user gesture has occurred
    /// in the page (Conversation Mode is always entered via a
    /// click, so this is fine in practice).
    pub fn new() -> Result<Self, JsValue> {
        let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
        let document = window
            .document()
            .ok_or_else(|| JsValue::from_str("no document"))?;

        let media_source = MediaSource::new()?;
        let object_url = web_sys::Url::create_object_url_with_source(&media_source)?;

        let audio: HtmlAudioElement = document
            .create_element("audio")?
            .dyn_into::<HtmlAudioElement>()?;
        audio.set_src(&object_url);
        // Hidden \u2014 the bubble UI provides the controls. Keeping
        // the element in the DOM is required for the browser to
        // schedule playback; `display:none` does not break audio.
        audio.set_attribute("style", "display:none").ok();
        document
            .body()
            .ok_or_else(|| JsValue::from_str("no body"))?
            .append_child(&audio)?;

        let state = Rc::new(RefCell::new(PlayerState {
            source_buffer: None,
            queue: VecDeque::new(),
            end_pending: false,
            stopped: false,
            _on_source_open: None,
            _on_update_end: None,
            _on_ended: None,
        }));

        // `sourceopen` fires once the media element + MediaSource
        // are wired together; only at that point can we add the
        // SourceBuffer. We immediately drain anything that landed
        // in the queue while we waited.
        let ms_for_open = media_source.clone();
        let ms_for_update = media_source.clone();
        let state_clone = state.clone();
        let on_source_open = Closure::<dyn FnMut()>::new(move || {
            match ms_for_open.add_source_buffer(AUDIO_MIME) {
                Ok(sb) => {
                    // Wire updateend before storing so the very
                    // first append's completion is observed.
                    let state_for_cb = state_clone.clone();
                    let ms_for_cb = ms_for_update.clone();
                    let on_update_end = Closure::<dyn FnMut()>::new(move || {
                        pump_queue(&ms_for_cb, &state_for_cb);
                    });
                    sb.set_onupdateend(Some(on_update_end.as_ref().unchecked_ref()));
                    let mut s = state_clone.borrow_mut();
                    s.source_buffer = Some(sb);
                    s._on_update_end = Some(on_update_end);
                    drop(s);
                    pump_queue(&ms_for_update, &state_clone);
                }
                Err(e) => {
                    web_sys::console::error_2(
                        &JsValue::from_str("MediaSourcePlayer: add_source_buffer failed"),
                        &e,
                    );
                }
            }
        });
        media_source.set_onsourceopen(Some(on_source_open.as_ref().unchecked_ref()));
        state.borrow_mut()._on_source_open = Some(on_source_open);

        Ok(Self {
            media_source,
            audio,
            object_url: Rc::new(object_url),
            state,
        })
    }

    /// Append a chunk of MP3 bytes. Safe to call before the
    /// `MediaSource` has opened and before previous appends have
    /// drained — chunks are queued internally and pumped in
    /// order from the `updateend` callback.
    ///
    /// No-op once [`Self::stop`] has been called.
    pub fn append(&self, bytes: &[u8]) -> Result<(), JsValue> {
        let mut s = self.state.borrow_mut();
        if s.stopped {
            return Ok(());
        }
        s.queue.push_back(bytes.to_vec());
        drop(s);
        pump_queue(&self.media_source, &self.state);
        Ok(())
    }

    /// Mark end-of-stream. The player will continue draining any
    /// queued chunks, then call `MediaSource.endOfStream()` so the
    /// audio element knows playback can stop after the last
    /// buffered frame. No-op when already ended or stopped.
    pub fn end(&self) -> Result<(), JsValue> {
        let mut s = self.state.borrow_mut();
        if s.stopped || s.end_pending {
            return Ok(());
        }
        s.end_pending = true;
        drop(s);
        // Try to finalize immediately; if the queue is non-empty
        // or an append is in flight, `pump_queue` will retry from
        // the next `updateend` callback.
        pump_queue(&self.media_source, &self.state);
        Ok(())
    }

    /// Pause playback at the current cursor.
    pub fn pause(&self) {
        let _ = self.audio.pause();
    }

    /// Resume playback. Returns `Err` only if the browser rejects
    /// the play promise (typically because no user gesture has
    /// been registered yet); the caller can ignore in most cases.
    pub fn play(&self) -> Result<(), JsValue> {
        // `play()` returns a `Promise`; we don't `.await` it
        // because most callers don't care, and the audio element
        // will surface errors via its own `error` event.
        let _ = self.audio.play()?;
        Ok(())
    }

    /// Halt playback and detach the audio element. The player
    /// becomes inert: subsequent [`Self::append`] / [`Self::end`]
    /// calls are no-ops. The object URL is revoked so the browser
    /// can reclaim the underlying buffer.
    pub fn stop(&self) {
        {
            let mut s = self.state.borrow_mut();
            if s.stopped {
                return;
            }
            s.stopped = true;
            s.queue.clear();
        }
        let _ = self.audio.pause();
        self.audio.set_src("");
        if let Some(parent) = self.audio.parent_node() {
            let _ = parent.remove_child(&self.audio);
        }
        let _ = web_sys::Url::revoke_object_url(&self.object_url);
    }

    /// Subscribe to playback-finished events. The callback fires
    /// when the audio element's `ended` event fires, which is
    /// after the last buffered MP3 frame has played out following
    /// an [`Self::end`] call.
    pub fn on_ended(&self, cb: Box<dyn Fn()>) {
        // `Box<dyn Fn()>` already satisfies `FnMut`, so we can hand
        // it straight to `Closure::new` without an extra wrapper.
        let closure = Closure::<dyn FnMut()>::new(cb);
        self.audio
            .set_onended(Some(closure.as_ref().unchecked_ref()));
        // Drop any previously installed handler and store the new
        // one so it lives as long as the player does.
        self.state.borrow_mut()._on_ended = Some(closure);
    }
}

/// Drain as many queued chunks as the `SourceBuffer` will accept
/// in one shot. MSE only allows one `appendBuffer` call at a time
/// per buffer, so we hand off to the `updateend` callback for the
/// rest. Once the queue empties and `end_pending` is set, calls
/// `MediaSource.endOfStream()` to signal the audio element it can
/// stop after the last buffered frame.
fn pump_queue(media_source: &MediaSource, state: &Rc<RefCell<PlayerState>>) {
    // Take the next chunk, if any, while we're not already
    // updating. We hold the borrow only across the queue probe +
    // sb.updating check so the JS callback is free to re-enter.
    let next = {
        let mut s = state.borrow_mut();
        if s.stopped {
            return;
        }
        let Some(sb) = s.source_buffer.as_ref() else {
            return; // SourceBuffer not yet attached
        };
        if sb.updating() {
            return; // wait for updateend
        }
        s.queue.pop_front()
    };

    if let Some(mut chunk) = next {
        // Re-borrow for the actual append; another updateend
        // can't fire until JS yields back, so this is safe.
        // `appendBuffer` takes `&mut [u8]` because the underlying
        // JS API may detach the ArrayBuffer; we own the chunk so
        // mutation is fine.
        let s = state.borrow();
        if let Some(sb) = s.source_buffer.as_ref()
            && let Err(e) = sb.append_buffer_with_u8_array(&mut chunk)
        {
            web_sys::console::error_2(
                &JsValue::from_str("MediaSourcePlayer: append_buffer failed"),
                &e,
            );
        }
        return;
    }

    // Queue empty — if end was requested, finalize now.
    let should_end = state.borrow().end_pending;
    if should_end && media_source.ready_state() == web_sys::MediaSourceReadyState::Open {
        if let Err(e) = media_source.end_of_stream() {
            web_sys::console::warn_2(
                &JsValue::from_str("MediaSourcePlayer: end_of_stream failed"),
                &e,
            );
        }
        // Clear the pending flag so a stray subsequent pump
        // doesn't try to finalize twice.
        state.borrow_mut().end_pending = false;
    }
}
