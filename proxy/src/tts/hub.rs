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

use super::AudioFormat;

/// One frame published to the live audio channel for a turn.
#[derive(Debug, Clone)]
pub enum TtsBroadcastFrame {
    /// A chunk of MP3 bytes. Concatenating every `Audio` frame in
    /// order rebuilds the same file the cache has on disk.
    ///
    /// `total_bytes_after` is the running total of audio bytes the
    /// orchestrator has emitted *including* this frame. Late
    /// subscribers use it to skip frames whose bytes are already
    /// covered by the cache snapshot they read at attach time —
    /// this is what makes the cache-then-live handoff in §5.1
    /// duplicate-free.
    Audio {
        /// Raw MP3 chunk bytes for this frame.
        bytes: Vec<u8>,
        /// Running cumulative byte count *including* this frame.
        total_bytes_after: u64,
    },
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

/// Registry entry for one live turn: the broadcast channel plus
/// the [`AudioFormat`] subscribers should announce to their clients
/// before relaying audio frames.
#[derive(Clone)]
struct HubEntry {
    tx: broadcast::Sender<TtsBroadcastFrame>,
    format: AudioFormat,
}

/// Registry of live per-turn TTS broadcasts. Cheap to clone (`Arc`
/// internally); the orchestrator and the HTTP layer share one.
#[derive(Clone, Default)]
pub struct TtsHub {
    inner: Arc<Mutex<HashMap<TurnId, HubEntry>>>,
}

impl TtsHub {
    /// Build an empty hub.
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a broadcast channel for `turn_id` declaring `format`.
    /// Returns a [`TtsBroadcaster`] that publishes into it. The
    /// declared format is exposed via [`Self::format_for`] so SSE
    /// subscribers can emit a leading `format` frame to the browser
    /// before relaying audio. Spec:
    /// `docs/cartesia-sonic-3-integration-spec.md` §6.4.
    ///
    /// If a channel already exists for `turn_id` (e.g. a stale entry
    /// from a crash or a retry), it is replaced — late subscribers
    /// to the old channel will see no further frames but won't error.
    pub fn open(&self, turn_id: TurnId, format: AudioFormat) -> TtsBroadcaster {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        self.inner.lock().expect("tts hub mutex poisoned").insert(
            turn_id.clone(),
            HubEntry {
                tx: tx.clone(),
                format,
            },
        );
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
            .map(|e| e.tx.subscribe())
    }

    /// Return the [`AudioFormat`] the broadcaster is producing for
    /// `turn_id`. `None` when no live broadcast exists.
    pub fn format_for(&self, turn_id: &str) -> Option<AudioFormat> {
        self.inner
            .lock()
            .expect("tts hub mutex poisoned")
            .get(turn_id)
            .map(|e| e.format)
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
    hub: Arc<Mutex<HashMap<TurnId, HubEntry>>>,
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
        let bcast = hub.open("turn-0001".into(), AudioFormat::Mp3_44100_128);
        let mut rx = hub.subscribe("turn-0001").expect("live");
        bcast.send(TtsBroadcastFrame::Audio {
            bytes: vec![1, 2, 3],
            total_bytes_after: 3,
        });
        bcast.send(TtsBroadcastFrame::Audio {
            bytes: vec![4, 5],
            total_bytes_after: 5,
        });
        bcast.finish();

        match rx.recv().await.unwrap() {
            TtsBroadcastFrame::Audio {
                bytes,
                total_bytes_after,
            } => {
                assert_eq!(bytes, vec![1, 2, 3]);
                assert_eq!(total_bytes_after, 3);
            }
            other => panic!("expected Audio, got {other:?}"),
        }
        match rx.recv().await.unwrap() {
            TtsBroadcastFrame::Audio {
                bytes,
                total_bytes_after,
            } => {
                assert_eq!(bytes, vec![4, 5]);
                assert_eq!(total_bytes_after, 5);
            }
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
        let bcast = hub.open("turn-0001".into(), AudioFormat::Mp3_44100_128);
        assert!(hub.is_live("turn-0001"));
        bcast.finish();
        assert!(!hub.is_live("turn-0001"));
        assert!(hub.subscribe("turn-0001").is_none());
    }

    #[tokio::test]
    async fn fail_emits_error_frame_and_removes() {
        let hub = TtsHub::new();
        let bcast = hub.open("turn-0001".into(), AudioFormat::Mp3_44100_128);
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
        let hub = TtsHub::new();
        {
            let _bcast = hub.open("turn-0001".into(), AudioFormat::Mp3_44100_128);
        }
        assert!(hub.is_live("turn-0001"));
    }

    #[tokio::test]
    async fn format_for_returns_declared_format() {
        let hub = TtsHub::new();
        let _bcast = hub.open("turn-0001".into(), AudioFormat::Pcm_S16LE_44100_Mono);
        assert_eq!(
            hub.format_for("turn-0001"),
            Some(AudioFormat::Pcm_S16LE_44100_Mono)
        );
        assert_eq!(hub.format_for("missing"), None);
    }
}
