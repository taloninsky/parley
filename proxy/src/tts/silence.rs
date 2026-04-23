//! MP3 silence splicing for paragraph-bounded TTS chunks.
//!
//! See `docs/paragraph-tts-chunking-spec.md` §3.1 / §3.4. The
//! orchestrator splices a short silence prefix in front of every
//! chunk's audio so that paragraph and inter-chunk boundaries land
//! with a small, prosodically pleasant pause.
//!
//! ## Design
//!
//! - The splicer holds **one** pre-computed silent MP3 frame (raw
//!   container bytes, exactly one frame long). For 44.1 kHz / 128
//!   kbps stereo MPEG-1 Layer III this is ~417 bytes covering
//!   ~26.1 ms (`1152 / 44100 * 1000`).
//! - To produce N milliseconds of silence the splicer concatenates
//!   `round(N / frame_duration_ms)` copies of that frame. Achievable
//!   precision is therefore one frame (~±13 ms either side of the
//!   request); the spec accepts ±15 ms.
//! - Splicing into an audio chunk is just byte concatenation: MP3
//!   frames are independently decodable and the silent frame's
//!   header/side-info encodes valid all-zero samples on its own.
//!
//! ## Why not generate silence on the fly?
//!
//! Encoding silence at runtime would pull in a libmp3lame-grade
//! encoder dependency for a one-line job. A pre-baked frame is
//! deterministic, byte-exact, and zero-allocation past startup.
//!
//! The bytes themselves are wired in at proxy startup
//! (`crate::tts::silence::SILENCE_FRAME_44100_128_STEREO` once the
//! file lands; until then callers pass an explicit slice into
//! [`SilenceSplicer::new`]). All splice/replication logic in this
//! module is fully unit-tested against synthetic frame bytes so it
//! does not depend on the real frame's content.

/// Splices pre-computed MP3 silence frames in front of audio chunks.
///
/// Cheap to clone — internally just an `Arc`-equivalent over a
/// borrowed byte slice and two integers. One instance per
/// orchestrator turn (or per process, if the silence frame is
/// shared via a `'static`).
#[derive(Debug, Clone)]
pub struct SilenceSplicer {
    /// One MP3 frame whose decoded PCM is all-zero samples. Must be
    /// a complete container frame for the target codec/sample rate.
    frame: &'static [u8],
    /// Decoded PCM duration of `frame` in milliseconds. Used to
    /// convert a requested silence duration into a frame count.
    frame_duration_ms: u32,
}

impl SilenceSplicer {
    /// Build a splicer around a single pre-computed silent MP3
    /// frame and its decoded PCM duration.
    ///
    /// Panics in debug builds if `frame` is empty or
    /// `frame_duration_ms` is zero — both indicate a misconfigured
    /// caller and would silently produce zero-length output.
    pub fn new(frame: &'static [u8], frame_duration_ms: u32) -> Self {
        debug_assert!(!frame.is_empty(), "silence frame must be non-empty");
        debug_assert!(frame_duration_ms > 0, "silence frame duration must be > 0");
        Self {
            frame,
            frame_duration_ms,
        }
    }

    /// Decoded PCM duration of one underlying silence frame.
    /// Exposed so callers can reason about achievable precision.
    pub fn frame_duration_ms(&self) -> u32 {
        self.frame_duration_ms
    }

    /// Produce approximately `duration_ms` of silence as raw
    /// container bytes. Returned length is
    /// `frame.len() * round(duration_ms / frame_duration_ms)`.
    ///
    /// `duration_ms == 0` returns an empty `Vec` (no allocation
    /// beyond the header).
    pub fn silence(&self, duration_ms: u32) -> Vec<u8> {
        if duration_ms == 0 {
            return Vec::new();
        }
        let frames = self.frame_count_for(duration_ms);
        let mut out = Vec::with_capacity(self.frame.len() * frames as usize);
        for _ in 0..frames {
            out.extend_from_slice(self.frame);
        }
        out
    }

    /// Prepend `silence_ms` of silence to `audio` and return the
    /// concatenated bytes. Convenience wrapper over [`Self::silence`]
    /// — the orchestrator uses this to splice silence into the
    /// first audio frame of each chunk before broadcast/cache.
    pub fn splice_with_silence(&self, silence_ms: u32, audio: &[u8]) -> Vec<u8> {
        if silence_ms == 0 {
            return audio.to_vec();
        }
        let frames = self.frame_count_for(silence_ms);
        let mut out = Vec::with_capacity(self.frame.len() * frames as usize + audio.len());
        for _ in 0..frames {
            out.extend_from_slice(self.frame);
        }
        out.extend_from_slice(audio);
        out
    }

    /// Round `duration_ms` to the nearest whole number of frames,
    /// with a floor of 1 frame for any non-zero request. The floor
    /// matters for short requests like `first_chunk_silence_ms = 100`
    /// when the frame duration is 26 ms: requesting 100 ms must
    /// produce *some* silence (4 frames ≈ 104 ms), not zero.
    fn frame_count_for(&self, duration_ms: u32) -> u32 {
        let half = self.frame_duration_ms / 2;
        // Round-half-to-even isn't worth the complexity here;
        // standard half-up rounding is fine.
        let n = (duration_ms + half) / self.frame_duration_ms;
        n.max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 8-byte synthetic "frame" worth 26 ms of silence. The splicer
    /// doesn't care what the bytes mean — it only replicates them.
    /// Real wiring uses a 417-byte 44.1 kHz/128 kbps stereo MP3
    /// silence frame loaded from `SILENCE_FRAME_44100_128_STEREO`
    /// once that constant is committed.
    const FAKE_FRAME: &[u8] = b"FRAMExxx";
    const FAKE_FRAME_MS: u32 = 26;

    fn splicer() -> SilenceSplicer {
        SilenceSplicer::new(FAKE_FRAME, FAKE_FRAME_MS)
    }

    #[test]
    fn silence_zero_returns_empty() {
        let s = splicer();
        assert!(s.silence(0).is_empty());
    }

    #[test]
    fn silence_short_request_floors_at_one_frame() {
        // 5 ms requested, frame is 26 ms — must still emit 1 frame
        // (104 ms > 5 ms but better than dropping silence entirely).
        let out = splicer().silence(5);
        assert_eq!(out.len(), FAKE_FRAME.len());
        assert_eq!(&out, FAKE_FRAME);
    }

    #[test]
    fn silence_rounds_to_nearest_frame() {
        let s = splicer();
        // 100 ms / 26 ms = 3.846 → rounds up to 4 frames (~104 ms).
        let out = s.silence(100);
        assert_eq!(out.len(), 4 * FAKE_FRAME.len());
        // 39 ms / 26 ms = 1.5 → half-up rounds to 2 frames.
        let out = s.silence(39);
        assert_eq!(out.len(), 2 * FAKE_FRAME.len());
        // 12 ms / 26 ms = 0.46 → rounds down to 0, but floor lifts to 1.
        let out = s.silence(12);
        assert_eq!(out.len(), FAKE_FRAME.len());
    }

    #[test]
    fn silence_long_request_concatenates_correctly() {
        let s = splicer();
        // 500 ms / 26 ms = 19.23 → 19 frames.
        let out = s.silence(500);
        assert_eq!(out.len(), 19 * FAKE_FRAME.len());
        // Bytes must repeat the source exactly with no separators.
        for chunk in out.chunks(FAKE_FRAME.len()) {
            assert_eq!(chunk, FAKE_FRAME);
        }
    }

    #[test]
    fn splice_with_zero_silence_returns_audio_only() {
        let audio = b"AUDIO_BYTES";
        let out = splicer().splice_with_silence(0, audio);
        assert_eq!(out, audio);
    }

    #[test]
    fn splice_prepends_silence_then_audio() {
        let audio = b"AUDIO_BYTES";
        let out = splicer().splice_with_silence(500, audio);
        // Header = 19 frames, tail = audio.
        let header_len = 19 * FAKE_FRAME.len();
        assert_eq!(out.len(), header_len + audio.len());
        assert_eq!(&out[header_len..], audio);
        // Header is exactly the silence stream from `silence(500)`.
        assert_eq!(&out[..header_len], splicer().silence(500).as_slice());
    }

    #[test]
    fn frame_duration_is_exposed_for_caller_diagnostics() {
        assert_eq!(splicer().frame_duration_ms(), FAKE_FRAME_MS);
    }
}
