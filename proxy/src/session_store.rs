//! On-disk persistence for [`ConversationSession`].
//!
//! Sessions are written one JSON file per session, named
//! `{session_id}.json` inside the configured store directory
//! (typically `~/.parley/sessions/`). JSON over TOML because turn
//! arrays nest cleanly and we never hand-edit the file.
//!
//! The trait abstraction lets tests swap in an in-memory store and
//! lets a future slice add an alternative backend (sqlite, blob
//! store) without touching the HTTP surface.
//!
//! ## Path safety
//!
//! Session ids come from the network. We refuse to write or read any
//! id that could escape the store directory: empty strings, dot/
//! double-dot segments, anything containing path separators, NUL
//! bytes, or non-printable ASCII control characters. Allowed
//! characters: ASCII alphanumerics, `_`, `-`, `.` (interior only).
//! This deliberately rejects unicode — keeps the file-name policy
//! identical across platforms.
//!
//! Spec reference: `docs/conversation-mode-spec.md` §9 (persistence).

#![allow(dead_code)] // Some helpers are exercised only through HTTP integration tests.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use parley_core::conversation::{ConversationSession, SessionId};
use thiserror::Error;

/// All ways a session-store call can fail.
#[derive(Debug, Error)]
pub enum SessionStoreError {
    /// The session id violates the path-safety policy. Surfaced as
    /// `400 Bad Request` by the HTTP layer.
    #[error(
        "invalid session id '{0}': must be ASCII alphanumeric / '-' / '_' / '.', no path separators"
    )]
    InvalidId(String),
    /// `load` was called for an id that doesn't exist on disk.
    /// Surfaced as `404 Not Found`.
    #[error("session '{0}' not found")]
    NotFound(SessionId),
    /// I/O failure (permissions, disk full, broken file).
    #[error("io error for session store at {path}: {source}")]
    Io {
        /// File or directory the operation touched.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// Stored JSON failed to deserialize. Either the file was edited
    /// by hand or the schema drifted.
    #[error("session '{id}' on disk is malformed: {source}")]
    Decode {
        /// Session id that failed to decode.
        id: SessionId,
        /// Underlying serde error.
        #[source]
        source: serde_json::Error,
    },
}

/// Storage backend for `ConversationSession`s. Async because the
/// real implementation does blocking I/O on a tokio worker; tests
/// that use the in-memory store also implement this trait so the
/// HTTP layer treats them identically.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Persist `session`. Overwrites any existing file with the same
    /// id (sessions are append-mostly; the in-memory truth is
    /// authoritative).
    async fn save(&self, session: &ConversationSession) -> Result<(), SessionStoreError>;

    /// Load the session with `id`. Returns `NotFound` if no such
    /// file exists.
    async fn load(&self, id: &str) -> Result<ConversationSession, SessionStoreError>;

    /// Enumerate all session ids currently in the store. Order is
    /// implementation-defined — the HTTP layer sorts before
    /// returning.
    async fn list(&self) -> Result<Vec<SessionId>, SessionStoreError>;
}

/// Filesystem-backed implementation. One file per session.
pub struct FsSessionStore {
    root: PathBuf,
}

impl FsSessionStore {
    /// Build a store rooted at `root`. The directory is created on
    /// first write — construction itself does no I/O.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory this store writes into. Useful for log lines
    /// and tests that want to inspect the on-disk layout.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, id: &str) -> Result<PathBuf, SessionStoreError> {
        validate_id(id)?;
        Ok(self.root.join(format!("{id}.json")))
    }

    fn ensure_root(&self) -> Result<(), SessionStoreError> {
        if !self.root.exists() {
            fs::create_dir_all(&self.root).map_err(|source| SessionStoreError::Io {
                path: self.root.clone(),
                source,
            })?;
        }
        Ok(())
    }
}

#[async_trait]
impl SessionStore for FsSessionStore {
    async fn save(&self, session: &ConversationSession) -> Result<(), SessionStoreError> {
        // Validate before doing any I/O so a bad id doesn't create
        // the root directory as a side effect.
        let path = self.path_for(&session.id)?;
        self.ensure_root()?;
        let body = serde_json::to_vec_pretty(session).map_err(|source| {
            // Serializing a Session shouldn't really fail — the
            // schema is closed and serde-derived — but if it does
            // we surface it as a decode error against the would-be
            // id rather than panicking.
            SessionStoreError::Decode {
                id: session.id.clone(),
                source,
            }
        })?;
        fs::write(&path, body).map_err(|source| SessionStoreError::Io { path, source })?;
        Ok(())
    }

    async fn load(&self, id: &str) -> Result<ConversationSession, SessionStoreError> {
        let path = self.path_for(id)?;
        let raw = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(SessionStoreError::NotFound(id.to_string()));
            }
            Err(source) => return Err(SessionStoreError::Io { path, source }),
        };
        serde_json::from_slice(&raw).map_err(|source| SessionStoreError::Decode {
            id: id.to_string(),
            source,
        })
    }

    async fn list(&self) -> Result<Vec<SessionId>, SessionStoreError> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let read_dir = fs::read_dir(&self.root).map_err(|source| SessionStoreError::Io {
            path: self.root.clone(),
            source,
        })?;
        let mut out = Vec::new();
        for entry in read_dir {
            let entry = entry.map_err(|source| SessionStoreError::Io {
                path: self.root.clone(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                // Defensive: only surface ids that round-trip through
                // our own validator. A hand-dropped file with a wild
                // name shouldn't poison the listing.
                if validate_id(stem).is_ok() {
                    out.push(stem.to_string());
                }
            }
        }
        Ok(out)
    }
}

/// Reject ids that are empty, contain path separators, dot segments,
/// NUL bytes, ASCII control characters, or non-allowed characters.
/// The allowed alphabet is `[A-Za-z0-9_\-.]`. A leading `.` is
/// rejected to keep dotfiles from masquerading as sessions; trailing
/// `.` is rejected to dodge a Windows quirk that strips it.
fn validate_id(id: &str) -> Result<(), SessionStoreError> {
    let bad = || SessionStoreError::InvalidId(id.to_string());
    if id.is_empty() {
        return Err(bad());
    }
    if id == "." || id == ".." {
        return Err(bad());
    }
    if id.starts_with('.') || id.ends_with('.') {
        return Err(bad());
    }
    for c in id.chars() {
        let ok = c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.';
        if !ok || c.is_ascii_control() {
            return Err(bad());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use parley_core::speaker::Speaker;
    use tempfile::TempDir;

    fn sample_session(id: &str) -> ConversationSession {
        ConversationSession::new(
            id,
            Speaker::ai_agent("ai-x", "X"),
            "scholar".to_string(),
            "m1".to_string(),
        )
    }

    // ── validate_id ────────────────────────────────────────────

    #[test]
    fn validate_id_accepts_normal_ids() {
        assert!(validate_id("sess-001").is_ok());
        assert!(validate_id("Session_42").is_ok());
        assert!(validate_id("a.b.c").is_ok());
        assert!(validate_id("X").is_ok());
    }

    #[test]
    fn validate_id_rejects_path_traversal_and_separators() {
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "trailing.",
            "a/b",
            "a\\b",
            "a b",
            "a:b",
            "a\0b",
            "a\nb",
            "naïve", // non-ASCII
        ] {
            assert!(validate_id(bad).is_err(), "expected '{bad}' to be rejected");
        }
    }

    // ── FsSessionStore ────────────────────────────────────────

    #[tokio::test]
    async fn save_then_load_roundtrips() {
        let tmp = TempDir::new().unwrap();
        let store = FsSessionStore::new(tmp.path());
        let session = sample_session("sess-1");
        store.save(&session).await.unwrap();
        let loaded = store.load("sess-1").await.unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.persona_history.len(), 1);
    }

    #[tokio::test]
    async fn save_creates_root_directory_lazily() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("nested").join("sessions");
        // Pre-condition: directory does not exist.
        assert!(!nested.exists());
        let store = FsSessionStore::new(&nested);
        store.save(&sample_session("only")).await.unwrap();
        assert!(nested.exists(), "root dir should have been created");
    }

    #[tokio::test]
    async fn load_unknown_id_returns_not_found() {
        let tmp = TempDir::new().unwrap();
        let store = FsSessionStore::new(tmp.path());
        let err = match store.load("missing").await {
            Err(e) => e,
            Ok(_) => panic!("expected NotFound"),
        };
        assert!(matches!(err, SessionStoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn invalid_id_is_rejected_without_touching_disk() {
        let tmp = TempDir::new().unwrap();
        let store = FsSessionStore::new(tmp.path().join("would-be-root"));
        let err = match store.load("../etc/passwd").await {
            Err(e) => e,
            Ok(_) => panic!("expected InvalidId"),
        };
        assert!(matches!(err, SessionStoreError::InvalidId(_)));
        // And the root was not created as a side effect.
        assert!(!tmp.path().join("would-be-root").exists());
    }

    #[tokio::test]
    async fn list_returns_only_well_named_json_files() {
        let tmp = TempDir::new().unwrap();
        let store = FsSessionStore::new(tmp.path());
        store.save(&sample_session("one")).await.unwrap();
        store.save(&sample_session("two")).await.unwrap();
        // Drop a foreign file in to make sure list ignores it.
        fs::write(tmp.path().join("README.md"), b"hi").unwrap();
        // And a wild-named .json file (skipped by the validator).
        fs::write(tmp.path().join("..hidden.json"), b"{}").unwrap();
        let mut got = store.list().await.unwrap();
        got.sort();
        assert_eq!(got, vec!["one".to_string(), "two".to_string()]);
    }

    #[tokio::test]
    async fn list_on_missing_root_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let store = FsSessionStore::new(tmp.path().join("nope"));
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn corrupt_file_surfaces_decode_error() {
        let tmp = TempDir::new().unwrap();
        let store = FsSessionStore::new(tmp.path());
        fs::create_dir_all(tmp.path()).unwrap();
        fs::write(tmp.path().join("broken.json"), b"{not valid json").unwrap();
        let err = match store.load("broken").await {
            Err(e) => e,
            Ok(_) => panic!("expected Decode"),
        };
        assert!(matches!(err, SessionStoreError::Decode { .. }));
    }

    #[tokio::test]
    async fn save_overwrites_existing_file() {
        let tmp = TempDir::new().unwrap();
        let store = FsSessionStore::new(tmp.path());
        let mut session = sample_session("dup");
        store.save(&session).await.unwrap();
        // Mutate then save again.
        session.append_user_turn("g".to_string(), "hello".to_string(), 1);
        store.save(&session).await.unwrap();
        let loaded = store.load("dup").await.unwrap();
        assert_eq!(loaded.turns.len(), 1);
    }
}
