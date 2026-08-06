pub mod error;
pub mod file_backend;
pub mod keyring_backend;

use std::collections::HashMap;

use error::Result;

pub const SERVICE_NAME: &str = "io.github.elementium";

/// localStorage keys that contain sensitive session data.
pub const SENSITIVE_KEYS: &[&str] = &[
    "mx_access_token",
    "mx_pickle_key",
    "mx_has_pickle_key",
    "mx_user_id",
    "mx_device_id",
    "mx_hs_url",
    "mx_is_guest",
];

/// Unified interface for secret storage backends.
pub trait SecretStore: Send + Sync {
    /// # Errors
    /// Returns an error if the backend fails to read the value.
    fn get(&self, key: &str) -> Result<Option<String>>;

    /// # Errors
    /// Returns an error if the backend fails to store the value.
    fn set(&self, key: &str, value: &str) -> Result<()>;

    /// # Errors
    /// Returns an error if the backend fails to delete the value.
    fn delete(&self, key: &str) -> Result<()>;

    /// # Errors
    /// Returns an error if the backend fails to enumerate stored values.
    fn get_all(&self) -> Result<HashMap<String, String>>;
}

/// Which backend is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BackendType {
    OsKeyring,
    EncryptedFile,
    NeedsSetup,
}

/// The active secret-storage backend, if one has been set up.
///
/// A closed enum instead of `Box<dyn SecretStore>`: there are exactly two concrete
/// backends and they're both known at compile time, so an enum dispatches without the
/// indirection/no-generics-across-the-call-site tradeoffs of a trait object. It also
/// doubles as the single source of truth for [`BackendType`] via [`SecretBackend::kind`]
/// -- callers no longer need to keep a backend and a separately-tracked `BackendType`
/// in sync by hand (previously two independent `Mutex`es in Tauri's `SecretStoreState`
/// that could in principle drift apart).
pub enum SecretBackend {
    Keyring(keyring_backend::KeyringBackend),
    File(file_backend::FileBackend),
}

impl SecretBackend {
    #[must_use]
    pub const fn kind(&self) -> BackendType {
        match self {
            Self::Keyring(_) => BackendType::OsKeyring,
            Self::File(_) => BackendType::EncryptedFile,
        }
    }

    /// # Errors
    /// Returns an error if the backend fails to read the value.
    pub fn get(&self, key: &str) -> Result<Option<String>> {
        match self {
            Self::Keyring(k) => k.get(key),
            Self::File(f) => f.get(key),
        }
    }

    /// # Errors
    /// Returns an error if the backend fails to store the value.
    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        match self {
            Self::Keyring(k) => k.set(key, value),
            Self::File(f) => f.set(key, value),
        }
    }

    /// # Errors
    /// Returns an error if the backend fails to delete the value.
    pub fn delete(&self, key: &str) -> Result<()> {
        match self {
            Self::Keyring(k) => k.delete(key),
            Self::File(f) => f.delete(key),
        }
    }

    /// # Errors
    /// Returns an error if the backend fails to enumerate stored values.
    pub fn get_all(&self) -> Result<HashMap<String, String>> {
        match self {
            Self::Keyring(k) => k.get_all(),
            Self::File(f) => f.get_all(),
        }
    }
}

/// Try to create the best available backend.
///
/// Returns `None` if no backend could be initialized (the frontend should prompt for
/// manual encrypted-file setup, i.e. [`BackendType::NeedsSetup`] -- represented here by
/// the absence of a backend rather than a separate enum variant tracked alongside it).
#[must_use]
pub fn create_backend() -> Option<SecretBackend> {
    match keyring_backend::KeyringBackend::try_new() {
        Ok(kb) => {
            tracing::info!("using OS keyring for secret storage");
            Some(SecretBackend::Keyring(kb))
        }
        Err(e) => {
            tracing::warn!("OS keyring unavailable ({e}), secrets need manual setup or will fall back to localStorage");
            None
        }
    }
}

/// Check if a given localStorage key is sensitive.
#[must_use]
pub fn is_sensitive_key(key: &str) -> bool {
    SENSITIVE_KEYS.contains(&key)
}
