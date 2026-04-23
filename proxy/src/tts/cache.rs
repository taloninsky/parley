//! On-disk MP3 cache for synthesized turns.
//!
//! Each turn writes its full audio stream to
//! `{root}/{session_id}/tts-cache/turn-{NNNN}.mp3` as the bytes
//! arrive. The cache file is the source of truth for replay (the
//! `/conversation/tts/{turn_id}/replay` route) and for late
//! subscribers that connect *after* a live stream is already in
//! progress.
//!
//! Spec: `docs/conversation-voice-slice-spec.md` §4.3.
//!
//! ## Concurrency model
//!
//! - One [`TtsCacheWriter`] per turn. The orchestrator owns it and
//!   drops it when the TTS stream finishes (success or failure).
//! - Many [`TtsCacheReader`]s per turn — readers may be created at
//!   any time, including before the writer has finished.
//! - We rely on the OS append-mode file handle for crash safety:
//!   partial files are still valid MP3 (decoders ignore truncated
//!   frames), so a mid-stream crash leaves a playable but
//!   short-by-some-frames artifact.

use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::fs;
use tokio::io::AsyncWriteExt;

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

    /// Path to the cache file for a single turn. Pure path
    /// arithmetic.
    fn turn_path(&self, session_id: &str, turn_id: &str) -> PathBuf {
        self.cache_dir(session_id).join(format!("{turn_id}.mp3"))
    }

    /// Open a writer for `(session_id, turn_id)`. Creates parent
    /// directories on demand. Truncates any pre-existing file so a
    /// retry overwrites a partial earlier attempt.
    pub async fn writer(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<TtsCacheWriter, CacheError> {
        let dir = self.cache_dir(session_id);
        fs::create_dir_all(&dir).await.map_err(|e| CacheError::Io {
            path: dir.clone(),
            message: e.to_string(),
        })?;
        let path = self.turn_path(session_id, turn_id);
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
        Ok(TtsCacheWriter { file, path })
    }

    /// Open a reader for `(session_id, turn_id)`. Returns `Ok(None)`
    /// when no cached file exists (the caller decides whether that
    /// is a 404 or a request to wait).
    pub async fn reader(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Option<TtsCacheReader>, CacheError> {
        let path = self.turn_path(session_id, turn_id);
        match fs::read(&path).await {
            Ok(bytes) => Ok(Some(TtsCacheReader { bytes })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CacheError::Io {
                path,
                message: e.to_string(),
            }),
        }
    }

    /// `true` when a cache file exists for `(session_id, turn_id)`.
    /// Cheap path probe — does not read file contents.
    pub async fn exists(&self, session_id: &str, turn_id: &str) -> bool {
        fs::try_exists(self.turn_path(session_id, turn_id))
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

    /// Flush and close the file. Drops `self` — call this instead of
    /// relying on `Drop` when you want flush errors surfaced.
    pub async fn finish(mut self) -> Result<(), CacheError> {
        self.file.flush().await.map_err(|e| CacheError::Io {
            path: self.path.clone(),
            message: e.to_string(),
        })
    }
}

/// In-memory snapshot of a cached turn. Constructed by
/// [`FsTtsCache::reader`]. The turn is small enough (handful of
/// kilobytes per sentence × handful of sentences) that loading it
/// fully avoids streaming complexity for the replay route.
pub struct TtsCacheReader {
    bytes: Vec<u8>,
}

impl TtsCacheReader {
    /// Return the cached bytes. Consumes the reader.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
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
    /// Public accessor for the resolved per-turn path. Useful for
    /// logging and tests; does not touch the filesystem.
    pub fn path_for(&self, session_id: &str, turn_id: &str) -> PathBuf {
        self.turn_path(session_id, turn_id)
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
    async fn round_trip_writes_and_reads_bytes() {
        let (c, _dir) = cache();
        let mut w = c.writer("sess", "turn-0001").await.unwrap();
        w.write(b"hello ").await.unwrap();
        w.write(b"world").await.unwrap();
        w.finish().await.unwrap();

        let r = c.reader("sess", "turn-0001").await.unwrap().unwrap();
        assert_eq!(r.as_slice(), b"hello world");
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
        let w = c.writer("sess", "turn-0001").await.unwrap();
        // Writer has created the file; even with no bytes yet it exists.
        assert!(c.exists("sess", "turn-0001").await);
        w.finish().await.unwrap();
        assert!(c.exists("sess", "turn-0001").await);
    }

    #[tokio::test]
    async fn writer_truncates_existing_file_on_reopen() {
        let (c, _dir) = cache();
        let mut w = c.writer("sess", "turn-0001").await.unwrap();
        w.write(b"first attempt with extra bytes").await.unwrap();
        w.finish().await.unwrap();

        // Re-open the same turn — simulates a retry after a TTS
        // failure mid-stream.
        let mut w = c.writer("sess", "turn-0001").await.unwrap();
        w.write(b"redo").await.unwrap();
        w.finish().await.unwrap();

        let r = c.reader("sess", "turn-0001").await.unwrap().unwrap();
        assert_eq!(r.as_slice(), b"redo");
    }

    #[tokio::test]
    async fn cache_dir_is_per_session() {
        let (c, _dir) = cache();
        let mut w1 = c.writer("sess-a", "turn-0001").await.unwrap();
        w1.write(b"alpha").await.unwrap();
        w1.finish().await.unwrap();
        let mut w2 = c.writer("sess-b", "turn-0001").await.unwrap();
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
    async fn path_for_nests_under_root_session_cache_dir() {
        let (c, _dir) = cache();
        let p = c.path_for("sess", "turn-0001");
        assert!(p.ends_with("sess/tts-cache/turn-0001.mp3"));
    }
}
