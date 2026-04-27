//! Provider credential storage and resolution.
//!
//! Spec reference: `docs/secrets-storage-spec.md` §4.
//!
//! # Design
//!
//! The [`SecretsManager`] is the only object in the proxy that touches
//! provider credentials. It exposes `resolve` / `set` / `delete` /
//! `rename` / `status` against a [`KeyStore`] backend (production:
//! `keyring::Entry`; tests: [`InMemoryKeyStore`]).
//!
//! Three pieces of state:
//!
//! 1. **`KeyStore`** — opaque key/value store keyed by an account string
//!    of the form `"<provider>/<credential>"`. The account format is an
//!    implementation detail of this module and never escapes.
//! 2. **`CredentialIndex`** — a non-secret JSON file at
//!    `<config_dir>/credentials.json` recording which `(provider,
//!    credential_name)` pairs exist. The index exists because `keyring`
//!    has no portable enumeration API; the index is the source of truth
//!    for "what credentials exist" and the keystore is the source of
//!    truth for "what their values are."
//! 3. **Env var lookup** — `PARLEY_<PROVIDER>_API_KEY` overrides the
//!    `default` credential only.
//!
//! # Concurrency
//!
//! `SecretsManager` holds a `Mutex<CredentialIndex>` for index access.
//! Keystore calls are made inside the lock to keep set/delete
//! atomic with respect to the index.
//!
//! # Logging
//!
//! No function in this module ever logs a secret value. Functions that
//! mention a credential refer to it as `"<provider>/<credential>"`.

use crate::providers::{ProviderCategory, ProviderId, REGISTRY, UnknownProvider};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use thiserror::Error;

/// The implicit credential name every provider always exposes.
pub const DEFAULT_CREDENTIAL: &str = "default";

/// Maximum length of a credential name (and of a stored key value).
const MAX_CREDENTIAL_NAME_LEN: usize = 32;
const MAX_KEY_LEN: usize = 512;

// ── KeyStore trait ─────────────────────────────────────────────────────

/// Abstraction over the underlying credential store. Production wires
/// this to `keyring::Entry`; tests use [`InMemoryKeyStore`]. This trait
/// is also the seam for any future external password-manager backend
/// (1Password CLI, Bitwarden CLI, …) — see
/// `docs/secrets-storage-spec.md` §9.2.
pub trait KeyStore: Send + Sync {
    /// Read the secret stored under `account`. Returns `Ok(None)` when
    /// the account does not exist; `Err` for backend failures (e.g.
    /// keychain access denied).
    fn get(&self, account: &str) -> Result<Option<String>, KeyStoreError>;

    /// Write `value` under `account`, replacing any existing value.
    fn set(&self, account: &str, value: &str) -> Result<(), KeyStoreError>;

    /// Remove `account`. Idempotent: removing an absent account is not
    /// an error.
    fn delete(&self, account: &str) -> Result<(), KeyStoreError>;
}

/// Backend error from a [`KeyStore`] call. Wraps the underlying message
/// without including any secret material.
#[derive(Debug, Error)]
#[error("keystore error on account {account:?}: {message}")]
pub struct KeyStoreError {
    /// Account string at the time of failure.
    pub account: String,
    /// Backend message (does not include secret values).
    pub message: String,
}

impl KeyStoreError {
    /// Construct a backend error for the given account.
    pub fn new(account: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            account: account.into(),
            message: message.into(),
        }
    }
}

// ── KeyringStore (production OS-backed implementation) ────────────────

/// Constant service name under which all Parley credentials are stored.
const KEYRING_SERVICE: &str = "parley";

/// Production [`KeyStore`] backed by the OS-native keystore via the
/// `keyring` crate (Windows Credential Manager / macOS Keychain / Linux
/// Secret Service). All entries live under the constant service name
/// [`KEYRING_SERVICE`]; the account string carries `"<provider>/<credential>"`.
pub struct KeyringStore;

impl KeyringStore {
    /// Construct. There is no per-instance state.
    pub fn new() -> Self {
        Self
    }

    fn entry(&self, account: &str) -> Result<keyring::Entry, KeyStoreError> {
        keyring::Entry::new(KEYRING_SERVICE, account)
            .map_err(|e| KeyStoreError::new(account, format!("entry init: {e}")))
    }
}

impl Default for KeyringStore {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyStore for KeyringStore {
    fn get(&self, account: &str) -> Result<Option<String>, KeyStoreError> {
        let entry = self.entry(account)?;
        match entry.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(KeyStoreError::new(account, format!("get: {e}"))),
        }
    }

    fn set(&self, account: &str, value: &str) -> Result<(), KeyStoreError> {
        let entry = self.entry(account)?;
        entry
            .set_password(value)
            .map_err(|e| KeyStoreError::new(account, format!("set: {e}")))
    }

    fn delete(&self, account: &str) -> Result<(), KeyStoreError> {
        let entry = self.entry(account)?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()), // idempotent
            Err(e) => Err(KeyStoreError::new(account, format!("delete: {e}"))),
        }
    }
}

// ── In-memory KeyStore (for tests and the no-keyring fallback) ─────────/// In-memory [`KeyStore`] used by tests. Public so HTTP-layer tests can
/// construct one without depending on the real OS keystore.
#[derive(Debug, Default)]
pub struct InMemoryKeyStore {
    inner: Mutex<BTreeMap<String, String>>,
}

impl InMemoryKeyStore {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl KeyStore for InMemoryKeyStore {
    fn get(&self, account: &str) -> Result<Option<String>, KeyStoreError> {
        Ok(self.inner.lock().unwrap().get(account).cloned())
    }

    fn set(&self, account: &str, value: &str) -> Result<(), KeyStoreError> {
        self.inner
            .lock()
            .unwrap()
            .insert(account.to_string(), value.to_string());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<(), KeyStoreError> {
        self.inner.lock().unwrap().remove(account);
        Ok(())
    }
}

// ── Credential index (non-secret) ──────────────────────────────────────

/// On-disk listing of which named credentials exist for each provider.
/// Persisted as plain JSON; never contains secret values.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CredentialIndex {
    /// Map of provider id → set of credential names known to exist.
    /// Stored as a sorted vector for deterministic on-disk output.
    #[serde(default)]
    providers: BTreeMap<String, Vec<String>>,
}

impl CredentialIndex {
    /// Read the index from `path`. A missing file yields an empty
    /// index (not an error) — fresh installs have no credentials yet.
    fn load(path: &Path) -> Result<Self, SecretsError> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| SecretsError::Index {
                path: path.to_path_buf(),
                message: format!("parse error: {e}"),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CredentialIndex::default()),
            Err(e) => Err(SecretsError::Index {
                path: path.to_path_buf(),
                message: format!("read error: {e}"),
            }),
        }
    }

    /// Write the index to `path` atomically (write-then-rename).
    fn save(&self, path: &Path) -> Result<(), SecretsError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| SecretsError::Index {
                path: path.to_path_buf(),
                message: format!("create dir error: {e}"),
            })?;
        }
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_string_pretty(self).map_err(|e| SecretsError::Index {
            path: path.to_path_buf(),
            message: format!("serialize error: {e}"),
        })?;
        std::fs::write(&tmp, body).map_err(|e| SecretsError::Index {
            path: tmp.clone(),
            message: format!("write error: {e}"),
        })?;
        std::fs::rename(&tmp, path).map_err(|e| SecretsError::Index {
            path: path.to_path_buf(),
            message: format!("rename error: {e}"),
        })?;
        Ok(())
    }

    /// Add `(provider, credential)`; idempotent.
    fn add(&mut self, provider: ProviderId, credential: &str) {
        let entry = self
            .providers
            .entry(provider.as_str().to_string())
            .or_default();
        if !entry.iter().any(|n| n == credential) {
            entry.push(credential.to_string());
            entry.sort();
        }
    }

    /// Remove `(provider, credential)`; idempotent.
    fn remove(&mut self, provider: ProviderId, credential: &str) {
        if let Some(entry) = self.providers.get_mut(provider.as_str()) {
            entry.retain(|n| n != credential);
            if entry.is_empty() {
                self.providers.remove(provider.as_str());
            }
        }
    }

    /// Names known to exist for `provider`, sorted.
    fn names_for(&self, provider: ProviderId) -> Vec<String> {
        self.providers
            .get(provider.as_str())
            .cloned()
            .unwrap_or_default()
    }
}

// ── Env-var lookup abstraction ─────────────────────────────────────────

/// Reads environment variables. Production uses [`std::env::var`]; tests
/// can supply a deterministic implementation.
pub trait EnvLookup: Send + Sync {
    /// Return the value of `name`, or `None` if unset / empty.
    fn get(&self, name: &str) -> Option<String>;
}

/// Real env-var reader.
pub struct ProcessEnv;

impl EnvLookup for ProcessEnv {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    }
}

/// Test env-var reader backed by a static map.
#[derive(Default)]
pub struct StaticEnv {
    inner: BTreeMap<String, String>,
}

impl StaticEnv {
    /// Empty env.
    pub fn new() -> Self {
        Self::default()
    }
    /// Set `name = value`.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.inner.insert(name.into(), value.into());
    }
}

impl EnvLookup for StaticEnv {
    fn get(&self, name: &str) -> Option<String> {
        self.inner.get(name).cloned()
    }
}

// ── Errors ─────────────────────────────────────────────────────────────

/// Errors returned by [`SecretsManager`] operations.
#[derive(Debug, Error)]
pub enum SecretsError {
    /// Credential name failed validation.
    #[error("invalid credential name: {0}")]
    InvalidCredentialName(String),
    /// Key value failed validation (empty, too long, non-printable).
    #[error("invalid key: {0}")]
    InvalidKey(&'static str),
    /// Tried to operate on the implicit `default` credential in a way
    /// that's not allowed (e.g. rename).
    #[error("cannot rename or remove the default credential slot")]
    DefaultCredentialReserved,
    /// Rename target already exists.
    #[error("credential {0:?} already exists")]
    CredentialExists(String),
    /// Backend error.
    #[error(transparent)]
    KeyStore(#[from] KeyStoreError),
    /// Index file I/O / parse error.
    #[error("credential index error at {path}: {message}")]
    Index {
        /// Index file path.
        path: PathBuf,
        /// Underlying message.
        message: String,
    },
}

// ── Status report ──────────────────────────────────────────────────────

/// Source of a configured credential value.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialSource {
    /// Resolved from an environment variable.
    Env,
    /// Resolved from the OS keystore.
    Keystore,
}

/// Per-credential status row in the `/api/secrets/status` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CredentialStatus {
    /// Credential name (e.g. `"default"`, `"work"`).
    pub name: String,
    /// `true` when a value is resolvable; `false` when unset.
    pub configured: bool,
    /// Where the value came from. `None` when `configured == false`.
    pub source: Option<CredentialSource>,
    /// Set when the index lists the credential but the keystore has no
    /// matching entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<&'static str>,
}

/// Per-provider entry in the status response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProviderStatus {
    /// Stable id (e.g. `"anthropic"`).
    pub id: String,
    /// Human-readable name.
    pub display_name: String,
    /// Env var that overrides `default`.
    pub env_var: String,
    /// All credentials known for this provider, with `default` always
    /// listed first.
    pub credentials: Vec<CredentialStatus>,
}

/// Top-level `/api/secrets/status` response shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StatusReport {
    /// Providers grouped by category.
    pub categories: BTreeMap<&'static str, Vec<ProviderStatus>>,
    /// Startup-time keystore errors surfaced for the UI.
    pub errors: Vec<String>,
}

// ── SecretsManager ─────────────────────────────────────────────────────

/// All credential operations funnel through this object.
pub struct SecretsManager {
    keystore: Box<dyn KeyStore>,
    env: Box<dyn EnvLookup>,
    index: Mutex<CredentialIndex>,
    index_path: PathBuf,
    /// Errors captured at construction time (e.g. index file unreadable
    /// but recoverable). Surfaced via `status()`.
    startup_errors: Vec<String>,
}

impl SecretsManager {
    /// Construct from explicit dependencies. Used by tests and by
    /// production wiring in `main.rs`.
    pub fn new(keystore: Box<dyn KeyStore>, env: Box<dyn EnvLookup>, index_path: PathBuf) -> Self {
        let (index, startup_errors) = match CredentialIndex::load(&index_path) {
            Ok(i) => (i, Vec::new()),
            Err(e) => {
                // Degrade to empty index — the user can re-add credentials.
                (CredentialIndex::default(), vec![e.to_string()])
            }
        };
        Self {
            keystore,
            env,
            index: Mutex::new(index),
            index_path,
            startup_errors,
        }
    }

    /// Resolve `(provider, credential)` to a key value. Resolution
    /// order: env var (only for `default`) → keystore → `None`.
    pub fn resolve(&self, provider: ProviderId, credential: &str) -> Option<String> {
        if credential == DEFAULT_CREDENTIAL
            && let Some(v) = self.env.get(provider.env_var())
        {
            return Some(v);
        }
        let account = account_string(provider, credential);
        self.keystore.get(&account).ok().flatten()
    }

    /// Store `key` for `(provider, credential)`. Validates inputs,
    /// writes to the keystore, and updates the credential index.
    pub fn set(
        &self,
        provider: ProviderId,
        credential: &str,
        key: &str,
    ) -> Result<CredentialStatus, SecretsError> {
        validate_credential_name(credential)?;
        validate_key(key)?;

        let account = account_string(provider, credential);
        self.keystore.set(&account, key)?;

        let mut index = self.index.lock().unwrap();
        index.add(provider, credential);
        index.save(&self.index_path)?;
        drop(index);

        Ok(self.credential_status(provider, credential))
    }

    /// Remove `(provider, credential)`. Idempotent. The implicit
    /// `default` slot is never removed from the listing — only its
    /// `configured` flag flips.
    pub fn delete(
        &self,
        provider: ProviderId,
        credential: &str,
    ) -> Result<CredentialStatus, SecretsError> {
        validate_credential_name(credential)?;

        let account = account_string(provider, credential);
        self.keystore.delete(&account)?;

        let mut index = self.index.lock().unwrap();
        if credential != DEFAULT_CREDENTIAL {
            index.remove(provider, credential);
            index.save(&self.index_path)?;
        }
        drop(index);

        Ok(self.credential_status(provider, credential))
    }

    /// Rename a credential. Cannot be applied to or from `default`.
    pub fn rename(
        &self,
        provider: ProviderId,
        old: &str,
        new: &str,
    ) -> Result<CredentialStatus, SecretsError> {
        if old == DEFAULT_CREDENTIAL || new == DEFAULT_CREDENTIAL {
            return Err(SecretsError::DefaultCredentialReserved);
        }
        validate_credential_name(old)?;
        validate_credential_name(new)?;

        if self
            .resolve_keystore_only(provider, new)
            .map_err(SecretsError::from)?
            .is_some()
        {
            return Err(SecretsError::CredentialExists(new.to_string()));
        }

        let old_account = account_string(provider, old);
        let value = self.keystore.get(&old_account)?.ok_or_else(|| {
            // Treat "rename a non-existent credential" as a no-op-shaped
            // error using KeyStoreError; the HTTP layer will turn this
            // into 404.
            KeyStoreError::new(old_account.clone(), "credential not found".to_string())
        })?;

        let new_account = account_string(provider, new);
        self.keystore.set(&new_account, &value)?;
        self.keystore.delete(&old_account)?;

        let mut index = self.index.lock().unwrap();
        index.remove(provider, old);
        index.add(provider, new);
        index.save(&self.index_path)?;
        drop(index);

        Ok(self.credential_status(provider, new))
    }

    /// Build the full categorized status report.
    pub fn status(&self) -> StatusReport {
        let mut categories: BTreeMap<&'static str, Vec<ProviderStatus>> = BTreeMap::new();
        for cat in ProviderCategory::all() {
            categories.insert(cat.as_str(), Vec::new());
        }

        for descriptor in REGISTRY {
            let provider: ProviderId = descriptor
                .id
                .parse()
                .expect("REGISTRY ids must round-trip through ProviderId");
            let names = self.credential_names(provider);

            let credentials: Vec<CredentialStatus> = names
                .iter()
                .map(|n| self.credential_status(provider, n))
                .collect();

            // Multi-category providers (e.g. xAI: STT + TTS under one
            // bearer token) appear under each of their declared categories
            // with the same credential list. The UI surfaces one card per
            // (provider, category) pair; editing either view mutates the
            // same underlying credential.
            for cat in descriptor.categories {
                categories
                    .entry(cat.as_str())
                    .or_default()
                    .push(ProviderStatus {
                        id: descriptor.id.to_string(),
                        display_name: descriptor.display_name.to_string(),
                        env_var: descriptor.env_var.to_string(),
                        credentials: credentials.clone(),
                    });
            }
        }

        StatusReport {
            categories,
            errors: self.startup_errors.clone(),
        }
    }

    /// Status of a single credential. Used by HTTP handlers as the
    /// response body for `PUT` / `DELETE` / `rename`.
    pub fn credential_status(&self, provider: ProviderId, credential: &str) -> CredentialStatus {
        // Env override only applies to `default`.
        if credential == DEFAULT_CREDENTIAL
            && let Some(_v) = self.env.get(provider.env_var())
        {
            return CredentialStatus {
                name: credential.to_string(),
                configured: true,
                source: Some(CredentialSource::Env),
                warning: None,
            };
        }

        let account = account_string(provider, credential);
        match self.keystore.get(&account) {
            Ok(Some(_)) => CredentialStatus {
                name: credential.to_string(),
                configured: true,
                source: Some(CredentialSource::Keystore),
                warning: None,
            },
            Ok(None) => {
                let listed = self
                    .index
                    .lock()
                    .unwrap()
                    .names_for(provider)
                    .iter()
                    .any(|n| n == credential);
                let warning = if listed {
                    Some("missing_in_keystore")
                } else {
                    None
                };
                CredentialStatus {
                    name: credential.to_string(),
                    configured: false,
                    source: None,
                    warning,
                }
            }
            Err(_) => CredentialStatus {
                name: credential.to_string(),
                configured: false,
                source: None,
                warning: Some("keystore_error"),
            },
        }
    }

    // ── internals ──────────────────────────────────────────────────

    /// Names to display for a provider: always `default` first, then
    /// any additional named credentials from the index, sorted.
    fn credential_names(&self, provider: ProviderId) -> Vec<String> {
        let mut names = vec![DEFAULT_CREDENTIAL.to_string()];
        let extras = self.index.lock().unwrap().names_for(provider);
        for n in extras {
            if n != DEFAULT_CREDENTIAL {
                names.push(n);
            }
        }
        names
    }

    fn resolve_keystore_only(
        &self,
        provider: ProviderId,
        credential: &str,
    ) -> Result<Option<String>, KeyStoreError> {
        self.keystore.get(&account_string(provider, credential))
    }
}

// ── Validation ─────────────────────────────────────────────────────────

/// Validate a credential name against `[a-z0-9_-]{1,32}`.
fn validate_credential_name(name: &str) -> Result<(), SecretsError> {
    if name.is_empty() || name.len() > MAX_CREDENTIAL_NAME_LEN {
        return Err(SecretsError::InvalidCredentialName(name.to_string()));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err(SecretsError::InvalidCredentialName(name.to_string()));
    }
    Ok(())
}

/// Validate a key value: non-empty, printable ASCII, length-bounded.
fn validate_key(key: &str) -> Result<(), SecretsError> {
    if key.is_empty() {
        return Err(SecretsError::InvalidKey("empty"));
    }
    if key.len() > MAX_KEY_LEN {
        return Err(SecretsError::InvalidKey("too long"));
    }
    if !key.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(SecretsError::InvalidKey("non-printable or whitespace"));
    }
    Ok(())
}

/// Compose the keystore account string for `(provider, credential)`.
fn account_string(provider: ProviderId, credential: &str) -> String {
    format!("{}/{}", provider.as_str(), credential)
}

// Convert UnknownProvider for callers that parse provider strings from
// HTTP paths.
impl From<UnknownProvider> for SecretsError {
    fn from(value: UnknownProvider) -> Self {
        SecretsError::InvalidCredentialName(format!("unknown provider: {}", value.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn manager_with_env(env: StaticEnv) -> (SecretsManager, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("credentials.json");
        let m = SecretsManager::new(Box::new(InMemoryKeyStore::new()), Box::new(env), path);
        (m, dir)
    }

    fn manager() -> (SecretsManager, TempDir) {
        manager_with_env(StaticEnv::new())
    }

    // ── credential name validation ─────────────────────────────────

    #[test]
    fn credential_name_accepts_valid_examples() {
        for ok in [
            "default",
            "work",
            "a",
            "a_b-c1",
            "abcdefghijklmnopqrstuvwxyz012345",
        ] {
            assert!(
                validate_credential_name(ok).is_ok(),
                "{ok:?} should be valid"
            );
        }
    }

    #[test]
    fn credential_name_rejects_invalid_examples() {
        for bad in [
            "",
            "Default",                           // uppercase
            "with space",                        // space
            "with.dot",                          // dot
            "abcdefghijklmnopqrstuvwxyz0123456", // 33 chars
        ] {
            assert!(
                validate_credential_name(bad).is_err(),
                "{bad:?} should be invalid"
            );
        }
    }

    // ── key validation ─────────────────────────────────────────────

    #[test]
    fn key_validation_rejects_empty_and_oversize() {
        assert!(validate_key("").is_err());
        let huge = "a".repeat(MAX_KEY_LEN + 1);
        assert!(validate_key(&huge).is_err());
    }

    #[test]
    fn key_validation_rejects_whitespace_and_control() {
        assert!(validate_key("ab cd").is_err()); // space
        assert!(validate_key("ab\tcd").is_err()); // tab
        assert!(validate_key("ab\ncd").is_err()); // newline
    }

    #[test]
    fn key_validation_accepts_realistic_key_shape() {
        assert!(validate_key("sk-ant-abcDEF1234567890_-").is_ok());
    }

    // ── resolution ────────────────────────────────────────────────

    #[test]
    fn resolve_returns_none_when_nothing_set() {
        let (m, _d) = manager();
        assert_eq!(m.resolve(ProviderId::Anthropic, "default"), None);
    }

    #[test]
    fn resolve_returns_env_for_default_when_set() {
        let mut env = StaticEnv::new();
        env.set("PARLEY_ANTHROPIC_API_KEY", "from-env");
        let (m, _d) = manager_with_env(env);
        // Even with a keystore value present, env wins for `default`.
        m.set(ProviderId::Anthropic, "default", "from-keystore")
            .unwrap();
        assert_eq!(
            m.resolve(ProviderId::Anthropic, "default"),
            Some("from-env".into())
        );
    }

    #[test]
    fn resolve_falls_back_to_keystore_when_env_unset() {
        let (m, _d) = manager();
        m.set(ProviderId::Anthropic, "default", "from-keystore")
            .unwrap();
        assert_eq!(
            m.resolve(ProviderId::Anthropic, "default"),
            Some("from-keystore".into())
        );
    }

    #[test]
    fn resolve_ignores_env_for_non_default_credential() {
        let mut env = StaticEnv::new();
        env.set("PARLEY_ANTHROPIC_API_KEY", "from-env");
        let (m, _d) = manager_with_env(env);
        // No keystore entry for "work" — env must NOT leak into it.
        assert_eq!(m.resolve(ProviderId::Anthropic, "work"), None);
    }

    #[test]
    fn resolve_named_credential_from_keystore() {
        let (m, _d) = manager();
        m.set(ProviderId::Anthropic, "work", "work-key").unwrap();
        assert_eq!(
            m.resolve(ProviderId::Anthropic, "work"),
            Some("work-key".into())
        );
    }

    // ── set / delete / rename ─────────────────────────────────────

    #[test]
    fn set_then_delete_round_trip() {
        let (m, _d) = manager();
        m.set(ProviderId::Anthropic, "work", "k1").unwrap();
        assert!(m.resolve(ProviderId::Anthropic, "work").is_some());
        m.delete(ProviderId::Anthropic, "work").unwrap();
        assert!(m.resolve(ProviderId::Anthropic, "work").is_none());
    }

    #[test]
    fn set_overwrites_existing_value_without_duplicating_index() {
        let (m, _d) = manager();
        m.set(ProviderId::Anthropic, "work", "v1").unwrap();
        m.set(ProviderId::Anthropic, "work", "v2").unwrap();
        assert_eq!(m.resolve(ProviderId::Anthropic, "work"), Some("v2".into()));
        let names = m.credential_names(ProviderId::Anthropic);
        // Should be ["default", "work"] — no duplicates.
        assert_eq!(names, vec!["default", "work"]);
    }

    #[test]
    fn delete_is_idempotent_on_missing_credential() {
        let (m, _d) = manager();
        m.delete(ProviderId::Anthropic, "ghost").unwrap();
        m.delete(ProviderId::Anthropic, "ghost").unwrap();
    }

    #[test]
    fn delete_default_keeps_default_in_listing() {
        let (m, _d) = manager();
        m.set(ProviderId::Anthropic, "default", "k").unwrap();
        m.delete(ProviderId::Anthropic, "default").unwrap();
        let names = m.credential_names(ProviderId::Anthropic);
        assert!(names.contains(&"default".to_string()));
    }

    #[test]
    fn delete_named_credential_removes_from_listing() {
        let (m, _d) = manager();
        m.set(ProviderId::Anthropic, "work", "k").unwrap();
        m.delete(ProviderId::Anthropic, "work").unwrap();
        let names = m.credential_names(ProviderId::Anthropic);
        assert!(!names.iter().any(|n| n == "work"));
    }

    #[test]
    fn rename_moves_value_and_updates_index() {
        let (m, _d) = manager();
        m.set(ProviderId::Anthropic, "work", "k").unwrap();
        m.rename(ProviderId::Anthropic, "work", "personal").unwrap();
        assert_eq!(m.resolve(ProviderId::Anthropic, "work"), None);
        assert_eq!(
            m.resolve(ProviderId::Anthropic, "personal"),
            Some("k".into())
        );
        let names = m.credential_names(ProviderId::Anthropic);
        assert!(names.contains(&"personal".to_string()));
        assert!(!names.contains(&"work".to_string()));
    }

    #[test]
    fn rename_rejects_default_as_either_argument() {
        let (m, _d) = manager();
        m.set(ProviderId::Anthropic, "default", "k").unwrap();
        assert!(matches!(
            m.rename(ProviderId::Anthropic, "default", "x"),
            Err(SecretsError::DefaultCredentialReserved)
        ));
        m.set(ProviderId::Anthropic, "work", "k").unwrap();
        assert!(matches!(
            m.rename(ProviderId::Anthropic, "work", "default"),
            Err(SecretsError::DefaultCredentialReserved)
        ));
    }

    #[test]
    fn rename_rejects_existing_target() {
        let (m, _d) = manager();
        m.set(ProviderId::Anthropic, "work", "k1").unwrap();
        m.set(ProviderId::Anthropic, "personal", "k2").unwrap();
        assert!(matches!(
            m.rename(ProviderId::Anthropic, "work", "personal"),
            Err(SecretsError::CredentialExists(_))
        ));
    }

    // ── status ────────────────────────────────────────────────────

    #[test]
    fn status_lists_every_provider_with_default_row() {
        let (m, _d) = manager();
        let s = m.status();
        let llm = s.categories.get("llm").unwrap();
        let stt = s.categories.get("stt").unwrap();
        assert!(llm.iter().any(|p| p.id == "anthropic"));
        assert!(stt.iter().any(|p| p.id == "assemblyai"));
        for cat in s.categories.values() {
            for p in cat {
                assert_eq!(p.credentials[0].name, "default");
                assert!(!p.credentials[0].configured);
            }
        }
    }

    #[test]
    fn status_marks_env_backed_default_as_env() {
        let mut env = StaticEnv::new();
        env.set("PARLEY_ANTHROPIC_API_KEY", "from-env");
        let (m, _d) = manager_with_env(env);
        let s = m.status();
        let anthropic = s
            .categories
            .get("llm")
            .unwrap()
            .iter()
            .find(|p| p.id == "anthropic")
            .unwrap();
        let default = &anthropic.credentials[0];
        assert!(default.configured);
        assert_eq!(default.source, Some(CredentialSource::Env));
    }

    #[test]
    fn status_marks_keystore_default_as_keystore() {
        let (m, _d) = manager();
        m.set(ProviderId::Anthropic, "default", "k").unwrap();
        let s = m.status();
        let default = &s
            .categories
            .get("llm")
            .unwrap()
            .iter()
            .find(|p| p.id == "anthropic")
            .unwrap()
            .credentials[0];
        assert_eq!(default.source, Some(CredentialSource::Keystore));
    }

    #[test]
    fn status_lists_named_credentials_after_default() {
        let (m, _d) = manager();
        m.set(ProviderId::Anthropic, "work", "k").unwrap();
        m.set(ProviderId::Anthropic, "personal", "k").unwrap();
        let s = m.status();
        let creds = &s
            .categories
            .get("llm")
            .unwrap()
            .iter()
            .find(|p| p.id == "anthropic")
            .unwrap()
            .credentials;
        assert_eq!(creds[0].name, "default");
        let extras: Vec<&str> = creds[1..].iter().map(|c| c.name.as_str()).collect();
        assert_eq!(extras, vec!["personal", "work"]); // sorted
    }

    // ── index / keystore divergence ───────────────────────────────

    #[test]
    fn index_entry_without_keystore_value_reports_warning() {
        // Add to the index directly, then verify status reports the warning.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("credentials.json");
        let mut index = CredentialIndex::default();
        index.add(ProviderId::Anthropic, "stale");
        index.save(&path).unwrap();

        let m = SecretsManager::new(
            Box::new(InMemoryKeyStore::new()),
            Box::new(StaticEnv::new()),
            path,
        );
        let s = m.credential_status(ProviderId::Anthropic, "stale");
        assert!(!s.configured);
        assert_eq!(s.warning, Some("missing_in_keystore"));
    }

    // ── persistence ───────────────────────────────────────────────

    #[test]
    fn index_persists_across_manager_instances() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("credentials.json");

        let m1 = SecretsManager::new(
            Box::new(InMemoryKeyStore::new()),
            Box::new(StaticEnv::new()),
            path.clone(),
        );
        m1.set(ProviderId::Anthropic, "work", "k").unwrap();
        // (Keystore is in-memory and won't survive, but the index will.)

        let m2 = SecretsManager::new(
            Box::new(InMemoryKeyStore::new()),
            Box::new(StaticEnv::new()),
            path,
        );
        let names = m2.credential_names(ProviderId::Anthropic);
        assert!(names.contains(&"work".to_string()));
    }
}
