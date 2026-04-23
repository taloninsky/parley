//! Per-turn fan-out for in-flight TTS audio.
//!
//! The orchestrator pushes audio chunks here as ElevenLabs returns
//! them; the `/conversation/tts/{turn_id}` SSE route subscribes by
//! `turn_id` to relay them to the browser. Late subscribers (the
//! browser opens the audio sibling stream a moment after seeing
//! `tts_started`) miss whatever bytes have already aired, so the
//! SSE route is responsible for first reading the cache file (which
//! has every byte the broadcaster has emitted, by construction —
//! the orchestrator always writes to cache *before* sending) and
//! then attaching the live receiver to tail the rest. That handoff
//! lives in `conversation_api`; the hub just owns the channels.
//!
//! Spec: `docs/conversation-voice-slice-spec.md` §4.4 and §5.1.
//!
//! ## Lifecycle
//!
//! - The orchestrator calls [`TtsHub::open`] when it dispatches the
//!   first sentence of a turn. The returned [`TtsBroadcaster`] is
//!   used to publish frames; dropping it (or calling
//!   [`TtsBroadcaster::finish`]) closes the stream.
//! - HTTP subscribers call [`TtsHub::subscribe`]. `None` means
//!   "broadcast already finished or never started"; the route
//!   should fall back to cache replay.
//! - Channels are removed from the registry on `finish`. A
//!   subscriber that holds an existing receiver continues to drain
//!   buffered frames after removal.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use parley_core::conversation::TurnId;
use tokio::sync::broadcast;

/// One frame published to the live audio channel for a turn.
#[derive(Debug, Clone)]
pub enum TtsBroadcastFrame {
    /// A chunk of MP3 bytes. Concatenating every `Audio` frame in
    /// order rebuilds the same file the cache has on disk.
    Audio(Vec<u8>),
    /// Synthesis finished cleanly; subscribers can close.
    Done,
    /// Synthesis errored. Subscribers should close and the browser
    /// should fall back to text-only rendering.
    Error(String),
}

/// Capacity of the per-turn broadcast channel. Generous because
/// MP3 chunks are tiny (a few KB each) and the consumer (an SSE
/// pump) is fast; this is just slack so a slow first connect
/// doesn't drop frames before the subscriber catches up.
const BROADCAST_CAPACITY: usize = 256;

/// Registry of live per-turn TTS broadcasts. Cheap to clone (`Arc`
/// internally); the orchestrator and the HTTP layer share one.
#[derive(Clone, Default)]
pub struct TtsHub {
    inner: Arc<Mutex<HashMap<TurnId, broadcast::Sender<TtsBroadcastFrame>>>>,
}

impl TtsHub {
    /// Build an empty hub.
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a broadcast channel for `turn_id`. Returns a
    /// [`TtsBroadcaster`] that publishes into it. If a channel
    /// already exists for `turn_id` (e.g. a stale entry from a
    /// crash or a retry), it is replaced — late subscribers to the
    /// old channel will see no further frames but won't error.
    pub fn open(&self, turn_id: TurnId) -> TtsBroadcaster {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        self.inner
            .lock()
            .expect("tts hub mutex poisoned")
            .insert(turn_id.clone(), tx.clone());
        TtsBroadcaster {
            turn_id,
            tx,
            hub: self.inner.clone(),
        }
    }

    /// Subscribe to the live broadcast for `turn_id`. `None` when
    /// no live broadcast exists (synthesis hasn't started, already
    /// finished, or never happened).
    pub fn subscribe(&self, turn_id: &str) -> Option<broadcast::Receiver<TtsBroadcastFrame>> {
        self.inner
            .lock()
            .expect("tts hub mutex poisoned")
            .get(turn_id)
            .map(|tx| tx.subscribe())
    }

    /// `true` when a live broadcast is registered for `turn_id`.
    pub fn is_live(&self, turn_id: &str) -> bool {
        self.inner
            .lock()
            .expect("tts hub mutex poisoned")
            .contains_key(turn_id)
    }
}

/// Publishing handle returned by [`TtsHub::open`]. Send audio frames
/// via [`Self::send`]; call [`Self::finish`] to emit `Done` and
/// remove the entry from the hub.
pub struct TtsBroadcaster {
    turn_id: TurnId,
    tx: broadcast::Sender<TtsBroadcastFrame>,
    hub: Arc<Mutex<HashMap<TurnId, broadcast::Sender<TtsBroadcastFrame>>>>,
}

impl TtsBroadcaster {
    /// Publish one frame. Errors from the underlying channel
    /// (no live receivers) are swallowed — the cache is the source
    /// of truth, so a missed live frame is recoverable.
    pub fn send(&self, frame: TtsBroadcastFrame) {
        let _ = self.tx.send(frame);
    }

    /// Publish a terminal `Done` frame and remove this turn's
    /// entry from the hub. Any new [`TtsHub::subscribe`] call after
    /// this returns `None`; existing receivers still drain
    /// buffered frames.
    pub fn finish(self) {
        let _ = self.tx.send(TtsBroadcastFrame::Done);
        self.hub
            .lock()
            .expect("tts hub mutex poisoned")
            .remove(&self.turn_id);
    }

    /// Publish a terminal `Error` frame and remove the entry.
    /// Mirrors [`Self::finish`] for the failure path.
    pub fn fail(self, message: String) {
        let _ = self.tx.send(TtsBroadcastFrame::Error(message));
        self.hub
            .lock()
            .expect("tts hub mutex poisoned")
            .remove(&self.turn_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_then_subscribe_receives_frames_in_order() {
        let hub = TtsHub::new();
        let bcast = hub.open("turn-0001".into());
        let mut rx = hub.subscribe("turn-0001").expect("live");
        bcast.send(TtsBroadcastFrame::Audio(vec![1, 2, 3]));
        bcast.send(TtsBroadcastFrame::Audio(vec![4, 5]));
        bcast.finish();

        match rx.recv().await.unwrap() {
            TtsBroadcastFrame::Audio(b) => assert_eq!(b, vec![1, 2, 3]),
            other => panic!("expected Audio, got {other:?}"),
        }
        match rx.recv().await.unwrap() {
            TtsBroadcastFrame::Audio(b) => assert_eq!(b, vec![4, 5]),
            other => panic!("expected Audio, got {other:?}"),
        }
        match rx.recv().await.unwrap() {
            TtsBroadcastFrame::Done => {}
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscribe_unknown_turn_returns_none() {
        let hub = TtsHub::new();
        assert!(hub.subscribe("nope").is_none());
    }

    #[tokio::test]
    async fn finish_removes_entry_from_hub() {
        let hub = TtsHub::new();
        let bcast = hub.open("turn-0001".into());
        assert!(hub.is_live("turn-0001"));
        bcast.finish();
        assert!(!hub.is_live("turn-0001"));
        assert!(hub.subscribe("turn-0001").is_none());
    }

    #[tokio::test]
    async fn fail_emits_error_frame_and_removes() {
        let hub = TtsHub::new();
        let bcast = hub.open("turn-0001".into());
        let mut rx = hub.subscribe("turn-0001").expect("live");
        bcast.fail("nope".into());
        match rx.recv().await.unwrap() {
            TtsBroadcastFrame::Error(m) => assert_eq!(m, "nope"),
            other => panic!("expected Error, got {other:?}"),
        }
        assert!(!hub.is_live("turn-0001"));
    }

    #[tokio::test]
    async fn dropping_broadcaster_without_finish_keeps_entry() {
        // We deliberately don't auto-finish on drop — the
        // orchestrator only ever drops the broadcaster via
        // `finish` or `fail`. This test pins that contract so a
        // future Drop impl is a deliberate decision.
        let hub = TtsHub::new();
        {
            let _bcast = hub.open("turn-0001".into());
        }
        assert!(hub.is_live("turn-0001"));
    }
}
