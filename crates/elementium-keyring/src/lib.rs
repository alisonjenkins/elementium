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
    fn get(&self, key: &str) -> Result<Option<String>>;
    fn set(&self, key: &str, value: &str) -> Result<()>;
    fn delete(&self, key: &str) -> Result<()>;
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

/// Try to create the best available backend.
/// Returns `(backend, type)` or `NeedsSetup` if no backend could be initialized.
pub fn create_backend() -> (Option<Box<dyn SecretStore>>, BackendType) {
    match keyring_backend::KeyringBackend::try_new() {
        Ok(kb) => {
            tracing::info!("using OS keyring for secret storage");
            (Some(Box::new(kb)), BackendType::OsKeyring)
        }
        Err(e) => {
            tracing::warn!("OS keyring unavailable ({e}), secrets need manual setup or will fall back to localStorage");
            (None, BackendType::NeedsSetup)
        }
    }
}

/// Check if a given localStorage key is sensitive.
pub fn is_sensitive_key(key: &str) -> bool {
    SENSITIVE_KEYS.contains(&key)
}
