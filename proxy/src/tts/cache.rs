//! On-disk audio cache for synthesized turns.
//!
//! Each turn writes its full audio stream to
//! `{root}/{session_id}/tts-cache/turn-{NNNN}.{ext}` as the bytes
//! arrive, alongside a sidecar `turn-{NNNN}.fmt` text file recording
//! the [`AudioFormat`] tag (e.g. `mp3_44100_128`,
//! `pcm_s16le_44100_mono`). The cache file is the source of truth for
//! replay (the `/conversation/tts/{turn_id}/replay` route) and for
//! late subscribers that connect *after* a live stream is already in
//! progress.
//!
//! Spec: `docs/conversation-voice-slice-spec.md` §4.3 and
//! `docs/cartesia-sonic-3-integration-spec.md` §6.4.
//!
//! ## Concurrency model
//!
//! - One [`TtsCacheWriter`] per turn. The orchestrator owns it and
//!   drops it when the TTS stream finishes (success or failure).
//! - Many [`TtsCacheReader`]s per turn — readers may be created at
//!   any time, including before the writer has finished.
//! - We rely on the OS append-mode file handle for crash safety:
//!   partial files are still valid for both MP3 (decoders ignore
//!   truncated frames) and PCM (every byte is independently valid),
//!   so a mid-stream crash leaves a playable but short-by-some-bytes
//!   artifact.
//!
//! ## Sidecar format
//!
//! `{turn_id}.fmt` contains a single ASCII line: the
//! [`AudioFormat::tag`] string. We picked a sidecar over an
//! extension-only encoding so the format tag is unambiguous (e.g.
//! `pcm_s16le_44100_mono` does not fit in a file extension) and
//! grep-friendly. Legacy caches without a sidecar are interpreted as
//! MP3 (the only format we wrote before this slice).

use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use super::AudioFormat;

/// Filename of the format-tag sidecar. Lives next to the audio file
/// in the same per-turn directory; not bundled into the audio file
/// because both MP3 and raw PCM treat any extra bytes as either an
/// invalid frame or invalid samples.
const SIDECAR_SUFFIX: &str = "fmt";

/// Errors surfaced by cache I/O. Wrapped at the call site into
/// [`TtsError::Other`](super::TtsError::Other) when they cause a
/// synthesis to fail; otherwise logged and ignored (a missing
/// cache file just means replay returns 404).
#[derive(Debug, Error)]
pub enum CacheError {
    /// Filesystem I/O failure (open, write, mkdir, etc.).
    #[error("tts cache io error at {path}: {message}")]
    Io {
        /// Affected path.
        path: PathBuf,
        /// Underlying error message.
        message: String,
    },
}

/// Filesystem-backed TTS cache rooted at one directory. Cheap to
/// clone — holds only the root path.
#[derive(Clone, Debug)]
pub struct FsTtsCache {
    root: PathBuf,
}

impl FsTtsCache {
    /// Build a cache rooted at `root`. The directory is created lazily
    /// the first time a writer is opened — no I/O at construction.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Return the directory `{root}/{session_id}/tts-cache`. Pure path
    /// arithmetic — does not touch the filesystem.
    fn cache_dir(&self, session_id: &str) -> PathBuf {
        self.root.join(session_id).join("tts-cache")
    }

    /// Path to the audio cache file for one turn at `format`. Pure
    /// path arithmetic.
    fn turn_audio_path(&self, session_id: &str, turn_id: &str, format: AudioFormat) -> PathBuf {
        self.cache_dir(session_id)
            .join(format!("{turn_id}.{}", format.cache_extension()))
    }

    /// Path to the sidecar format-tag file for one turn. Pure path
    /// arithmetic.
    fn turn_sidecar_path(&self, session_id: &str, turn_id: &str) -> PathBuf {
        self.cache_dir(session_id)
            .join(format!("{turn_id}.{SIDECAR_SUFFIX}"))
    }

    /// Open a writer for `(session_id, turn_id)` at `format`.
    /// Creates parent directories on demand. Truncates any
    /// pre-existing audio file so a retry overwrites a partial
    /// earlier attempt. Also writes the sidecar `{turn_id}.fmt`
    /// containing the format tag so the reader knows which container
    /// to dispatch to. Spec: format-aware cache (Cartesia §6.4).
    pub async fn writer(
        &self,
        session_id: &str,
        turn_id: &str,
        format: AudioFormat,
    ) -> Result<TtsCacheWriter, CacheError> {
        let dir = self.cache_dir(session_id);
        fs::create_dir_all(&dir).await.map_err(|e| CacheError::Io {
            path: dir.clone(),
            message: e.to_string(),
        })?;

        // Write the sidecar first so a crash mid-write of the audio
        // file still leaves a recoverable format hint. The writer is
        // a fresh attempt — overwrite the sidecar even if it existed.
        let sidecar = self.turn_sidecar_path(session_id, turn_id);
        fs::write(&sidecar, format.tag().as_bytes())
            .await
            .map_err(|e| CacheError::Io {
                path: sidecar.clone(),
                message: e.to_string(),
            })?;

        let path = self.turn_audio_path(session_id, turn_id, format);
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .await
            .map_err(|e| CacheError::Io {
                path: path.clone(),
                message: e.to_string(),
            })?;
        Ok(TtsCacheWriter { file, path, format })
    }

    /// Open a reader for `(session_id, turn_id)`. Returns `Ok(None)`
    /// when no cached file exists (the caller decides whether that
    /// is a 404 or a request to wait). Reads the sidecar
    /// `{turn_id}.fmt` to recover the [`AudioFormat`]; a missing
    /// sidecar is treated as MP3 for backwards compatibility with
    /// pre-Cartesia caches.
    pub async fn reader(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Option<TtsCacheReader>, CacheError> {
        let format = self.read_sidecar_format(session_id, turn_id).await?;
        let path = self.turn_audio_path(session_id, turn_id, format);
        match fs::read(&path).await {
            Ok(bytes) => Ok(Some(TtsCacheReader { bytes, format })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CacheError::Io {
                path,
                message: e.to_string(),
            }),
        }
    }

    /// Resolve the cached format for `(session_id, turn_id)`. Reads
    /// the sidecar; falls back to [`AudioFormat::Mp3_44100_128`] for
    /// caches written before the sidecar landed.
    async fn read_sidecar_format(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<AudioFormat, CacheError> {
        let path = self.turn_sidecar_path(session_id, turn_id);
        match fs::read_to_string(&path).await {
            Ok(s) => {
                let tag = s.trim();
                Ok(AudioFormat::from_tag(tag).unwrap_or(AudioFormat::Mp3_44100_128))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AudioFormat::Mp3_44100_128),
            Err(e) => Err(CacheError::Io {
                path,
                message: e.to_string(),
            }),
        }
    }

    /// `true` when an audio cache file exists for this turn at any
    /// known format. Cheap path probe — does not read file contents.
    /// Probes both the new sidecar-determined extension and the
    /// legacy `.mp3` fallback so test callers that pre-date the
    /// sidecar still see the right answer.
    pub async fn exists(&self, session_id: &str, turn_id: &str) -> bool {
        let fmt = match self.read_sidecar_format(session_id, turn_id).await {
            Ok(f) => f,
            Err(_) => AudioFormat::Mp3_44100_128,
        };
        fs::try_exists(self.turn_audio_path(session_id, turn_id, fmt))
            .await
            .unwrap_or(false)
    }
}

/// Append-only writer for one turn's cache file. Drop the writer to
/// close the file; explicit `finish()` is provided for callers that
/// want to surface flush errors instead of silently dropping them.
pub struct TtsCacheWriter {
    file: fs::File,
    path: PathBuf,
    format: AudioFormat,
}

impl TtsCacheWriter {
    /// Append `chunk` to the cache file. Errors propagate so the
    /// orchestrator can decide whether the failure is fatal for the
    /// turn (typically: log + continue, since a degraded cache only
    /// affects replay).
    pub async fn write(&mut self, chunk: &[u8]) -> Result<(), CacheError> {
        self.file
            .write_all(chunk)
            .await
            .map_err(|e| CacheError::Io {
                path: self.path.clone(),
                message: e.to_string(),
            })
    }

    /// Format the writer was opened with. Logged at the call site so
    /// we can correlate cache files with their producing provider.
    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// Flush and close the file. Drops `self` — call this instead of
    /// relying on `Drop` when you want flush errors surfaced.
    pub async fn finish(mut self) -> Result<(), CacheError> {
        self.file.flush().await.map_err(|e| CacheError::Io {
            path: self.path.clone(),
            message: e.to_string(),
        })
    }
}

/// In-memory snapshot of a cached turn, plus the format the bytes
/// are encoded in. Constructed by [`FsTtsCache::reader`]. The turn
/// is small enough (handful of kilobytes per sentence × handful of
/// sentences for MP3, ~88 KB/s for PCM) that loading it fully avoids
/// streaming complexity for the replay route.
pub struct TtsCacheReader {
    bytes: Vec<u8>,
    format: AudioFormat,
}

impl TtsCacheReader {
    /// Format the cached bytes are encoded in. Used by the replay
    /// endpoint to pick a content-type and (for PCM) prepend a
    /// streaming WAV header.
    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// Return the cached bytes. Consumes the reader.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Return both the cached bytes and their format. Consumes the
    /// reader. Convenience for callers (replay, late subscribers)
    /// that need both.
    pub fn into_parts(self) -> (Vec<u8>, AudioFormat) {
        (self.bytes, self.format)
    }

    /// Borrow the cached bytes without consuming the reader.
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Cached length in bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// `true` when the cached file is empty.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// `Path` accessor used by tests + diagnostic logging in the proxy.
impl FsTtsCache {
    /// Public accessor for the resolved per-turn audio path at
    /// `format`. Useful for logging and tests; does not touch the
    /// filesystem.
    pub fn path_for(&self, session_id: &str, turn_id: &str, format: AudioFormat) -> PathBuf {
        self.turn_audio_path(session_id, turn_id, format)
    }

    /// Public accessor for the cache root.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cache() -> (FsTtsCache, TempDir) {
        let dir = TempDir::new().unwrap();
        let cache = FsTtsCache::new(dir.path().to_path_buf());
        (cache, dir)
    }

    #[tokio::test]
    async fn round_trip_writes_and_reads_bytes_mp3() {
        let (c, _dir) = cache();
        let mut w = c
            .writer("sess", "turn-0001", AudioFormat::Mp3_44100_128)
            .await
            .unwrap();
        w.write(b"hello ").await.unwrap();
        w.write(b"world").await.unwrap();
        w.finish().await.unwrap();

        let r = c.reader("sess", "turn-0001").await.unwrap().unwrap();
        assert_eq!(r.format(), AudioFormat::Mp3_44100_128);
        assert_eq!(r.as_slice(), b"hello world");
    }

    #[tokio::test]
    async fn round_trip_writes_and_reads_bytes_pcm() {
        let (c, _dir) = cache();
        let mut w = c
            .writer("sess", "turn-0001", AudioFormat::Pcm_S16LE_44100_Mono)
            .await
            .unwrap();
        w.write(&[0x01, 0x00, 0x02, 0x00]).await.unwrap();
        w.write(&[0xff, 0x7f]).await.unwrap();
        w.finish().await.unwrap();

        let r = c.reader("sess", "turn-0001").await.unwrap().unwrap();
        assert_eq!(r.format(), AudioFormat::Pcm_S16LE_44100_Mono);
        assert_eq!(r.as_slice(), &[0x01, 0x00, 0x02, 0x00, 0xff, 0x7f]);
    }

    #[tokio::test]
    async fn writer_persists_format_sidecar() {
        let (c, _dir) = cache();
        let w = c
            .writer("sess", "turn-0001", AudioFormat::Pcm_S16LE_44100_Mono)
            .await
            .unwrap();
        w.finish().await.unwrap();

        let sidecar_path = c.turn_sidecar_path("sess", "turn-0001");
        let tag = fs::read_to_string(&sidecar_path).await.unwrap();
        assert_eq!(tag, "pcm_s16le_44100_mono");
    }

    #[tokio::test]
    async fn writer_truncates_existing_file_on_reopen() {
        let (c, _dir) = cache();
        let mut w = c
            .writer("sess", "turn-0001", AudioFormat::Mp3_44100_128)
            .await
            .unwrap();
        w.write(b"first attempt with extra bytes").await.unwrap();
        w.finish().await.unwrap();

        // Re-open the same turn — simulates a retry after a TTS
        // failure mid-stream. Same format.
        let mut w = c
            .writer("sess", "turn-0001", AudioFormat::Mp3_44100_128)
            .await
            .unwrap();
        w.write(b"redo").await.unwrap();
        w.finish().await.unwrap();

        let r = c.reader("sess", "turn-0001").await.unwrap().unwrap();
        assert_eq!(r.as_slice(), b"redo");
        assert_eq!(r.format(), AudioFormat::Mp3_44100_128);
    }

    #[tokio::test]
    async fn writer_reopen_with_different_format_updates_sidecar() {
        // Format change between attempts (e.g. provider switched
        // mid-session). The sidecar must follow the latest writer.
        let (c, _dir) = cache();
        let mut w = c
            .writer("sess", "turn-0001", AudioFormat::Mp3_44100_128)
            .await
            .unwrap();
        w.write(b"mp3-bytes").await.unwrap();
        w.finish().await.unwrap();

        let mut w = c
            .writer("sess", "turn-0001", AudioFormat::Pcm_S16LE_44100_Mono)
            .await
            .unwrap();
        w.write(&[0xaa, 0xbb]).await.unwrap();
        w.finish().await.unwrap();

        let r = c.reader("sess", "turn-0001").await.unwrap().unwrap();
        assert_eq!(r.format(), AudioFormat::Pcm_S16LE_44100_Mono);
        assert_eq!(r.as_slice(), &[0xaa, 0xbb]);
    }

    #[tokio::test]
    async fn reader_returns_none_when_audio_missing_even_if_sidecar_exists() {
        // Half-written cache: sidecar landed, audio file didn't.
        // The reader should see this as "no cache" (None), not error.
        let (c, _dir) = cache();
        let dir = c.cache_dir("sess");
        fs::create_dir_all(&dir).await.unwrap();
        fs::write(c.turn_sidecar_path("sess", "turn-0001"), "mp3_44100_128")
            .await
            .unwrap();
        assert!(c.reader("sess", "turn-0001").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn reader_falls_back_to_mp3_when_sidecar_missing() {
        // Legacy cache path: a `turn-0001.mp3` written before the
        // sidecar landed. Today's reader must recover it.
        let (c, _dir) = cache();
        let dir = c.cache_dir("sess");
        fs::create_dir_all(&dir).await.unwrap();
        fs::write(dir.join("turn-0001.mp3"), b"legacy-bytes")
            .await
            .unwrap();

        let r = c.reader("sess", "turn-0001").await.unwrap().unwrap();
        assert_eq!(r.format(), AudioFormat::Mp3_44100_128);
        assert_eq!(r.as_slice(), b"legacy-bytes");
    }

    #[tokio::test]
    async fn reader_returns_none_when_missing() {
        let (c, _dir) = cache();
        assert!(c.reader("sess", "turn-0001").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn exists_reflects_writer_lifecycle() {
        let (c, _dir) = cache();
        assert!(!c.exists("sess", "turn-0001").await);
        let w = c
            .writer("sess", "turn-0001", AudioFormat::Mp3_44100_128)
            .await
            .unwrap();
        assert!(c.exists("sess", "turn-0001").await);
        w.finish().await.unwrap();
        assert!(c.exists("sess", "turn-0001").await);
    }

    #[tokio::test]
    async fn cache_dir_is_per_session() {
        let (c, _dir) = cache();
        let mut w1 = c
            .writer("sess-a", "turn-0001", AudioFormat::Mp3_44100_128)
            .await
            .unwrap();
        w1.write(b"alpha").await.unwrap();
        w1.finish().await.unwrap();
        let mut w2 = c
            .writer("sess-b", "turn-0001", AudioFormat::Mp3_44100_128)
            .await
            .unwrap();
        w2.write(b"beta").await.unwrap();
        w2.finish().await.unwrap();

        assert_eq!(
            c.reader("sess-a", "turn-0001")
                .await
                .unwrap()
                .unwrap()
                .as_slice(),
            b"alpha"
        );
        assert_eq!(
            c.reader("sess-b", "turn-0001")
                .await
                .unwrap()
                .unwrap()
                .as_slice(),
            b"beta"
        );
    }

    #[tokio::test]
    async fn path_for_uses_format_extension() {
        let (c, _dir) = cache();
        let p = c.path_for("sess", "turn-0001", AudioFormat::Mp3_44100_128);
        assert!(p.ends_with("sess/tts-cache/turn-0001.mp3"));
        let p = c.path_for("sess", "turn-0001", AudioFormat::Pcm_S16LE_44100_Mono);
        assert!(p.ends_with("sess/tts-cache/turn-0001.pcm"));
    }
}
