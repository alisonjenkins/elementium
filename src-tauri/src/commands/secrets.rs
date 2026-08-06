// Every `#[tauri::command]` async fn below that takes a `State<'_, T>` parameter causes
// the `#[command]` macro to generate a sibling IPC-dispatch wrapper item in this module
// containing an internal match with an arm clippy flags as unreachable. That wrapper is
// framework codegen (not nested inside the fn item itself, so a function- or
// statement-scoped `#[allow]` cannot reach it — verified empirically), hence the
// module-level allow here rather than the usual per-item scoping.
#![allow(clippy::unreachable)]
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tauri::{State, command};

use elementium_keyring::{BackendType, SecretBackend, file_backend::FileBackend};

use super::LockExt;

/// Managed state for the secret store.
///
/// A single `Option<SecretBackend>` rather than a separate backend + `BackendType`
/// pair: the backend enum variant already determines its `BackendType` (via
/// `SecretBackend::kind`), so there is only one thing to keep locked and no way for
/// "which backend is active" and "what type does the frontend think is active" to
/// drift out of sync.
pub struct SecretStoreState {
    pub backend: Arc<Mutex<Option<SecretBackend>>>,
}

#[command]
pub async fn secret_get(
    key: String,
    state: State<'_, SecretStoreState>,
) -> Result<Option<String>, String> {
    let backend = state.backend.clone();
    tokio::task::spawn_blocking(move || {
        let guard = backend.lock_str()?;
        guard
            .as_ref()
            .map_or(Ok(None), |s| s.get(&key).map_err(|e| e.to_string()))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[command]
pub async fn secret_set(
    key: String,
    value: String,
    state: State<'_, SecretStoreState>,
) -> Result<(), String> {
    let backend = state.backend.clone();
    tokio::task::spawn_blocking(move || {
        let guard = backend.lock_str()?;
        // no backend, silently ignore
        guard
            .as_ref()
            .map_or(Ok(()), |s| s.set(&key, &value).map_err(|e| e.to_string()))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[command]
pub async fn secret_delete(key: String, state: State<'_, SecretStoreState>) -> Result<(), String> {
    let backend = state.backend.clone();
    tokio::task::spawn_blocking(move || {
        let guard = backend.lock_str()?;
        guard
            .as_ref()
            .map_or(Ok(()), |s| s.delete(&key).map_err(|e| e.to_string()))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[command]
pub async fn secret_get_all(
    state: State<'_, SecretStoreState>,
) -> Result<HashMap<String, String>, String> {
    let backend = state.backend.clone();
    tokio::task::spawn_blocking(move || {
        let guard = backend.lock_str()?;
        guard.as_ref().map_or_else(
            || Ok(HashMap::new()),
            |s| s.get_all().map_err(|e| e.to_string()),
        )
    })
    .await
    .map_err(|e| e.to_string())?
}

#[command]
pub async fn secret_get_backend_status(
    state: State<'_, SecretStoreState>,
) -> Result<BackendType, String> {
    let guard = state.backend.lock_str()?;
    Ok(guard.as_ref().map_or(BackendType::NeedsSetup, SecretBackend::kind))
}

#[command]
pub async fn secret_setup_file_backend(
    password: String,
    state: State<'_, SecretStoreState>,
) -> Result<(), String> {
    let backend_arc = state.backend.clone();

    tokio::task::spawn_blocking(move || {
        let file_backend = FileBackend::new(&password).map_err(|e| e.to_string())?;

        {
            let mut guard = backend_arc.lock_str()?;
            *guard = Some(SecretBackend::File(file_backend));
        }

        Ok(())
    })
    .await
    .map_err(|e: tokio::task::JoinError| e.to_string())?
}

#[cfg(test)]
mod secret_redaction_tests {
    use elementium_e2ee::{E2eeContext, E2eeOptions};
    use elementium_observability_test::LogCapture;

    use super::FileBackend;
    use elementium_keyring::SecretStore;

    /// Secret material used in this test. Chosen to be distinctive enough
    /// that an accidental substring collision with legitimate log output
    /// (level names, field names, etc.) is not plausible.
    const SECRET_KEY_MATERIAL: &[u8] = b"do-not-log-this-secret-3f9a7c21";
    const SECRET_PASSWORD: &str = "correct-horse-battery-staple-9f2e";
    const SECRET_STORED_VALUE: &str = "hunter2-access-token-1a2b3c4d";

    /// Regression/redaction test (SC-005): drive both the E2EE key-set path
    /// (`elementium-e2ee`) and the secret-store access path
    /// (`elementium-keyring`'s `FileBackend`) under the log-capture fixture,
    /// then assert that no captured event field contains the literal secret
    /// bytes/strings used here — only presence/size metadata (e.g.
    /// `key_len`) is allowed to appear.
    #[test]
    fn no_captured_event_field_contains_secret_material() {
        let capture = LogCapture::new();

        capture.run(|| {
            // E2EE key-set path.
            let ctx = E2eeContext::new(E2eeOptions::default());
            ctx.set_local_identity("alice");
            ctx.set_key("alice", 0, SECRET_KEY_MATERIAL);

            // Keyring secret-access path (encrypted file backend).
            if let Ok(backend) = FileBackend::new(SECRET_PASSWORD) {
                let _ = backend.set("mx_access_token", SECRET_STORED_VALUE);
                let _ = backend.get("mx_access_token");
            }
        });

        let secret_needle_key = String::from_utf8_lossy(SECRET_KEY_MATERIAL).into_owned();

        for event in capture.events() {
            for (field_name, value) in &event.fields {
                assert!(
                    !value.contains(&secret_needle_key),
                    "field {field_name:?} on event {:?} leaked E2EE key material: {value:?}",
                    event.name
                );
                assert!(
                    !value.contains(SECRET_PASSWORD),
                    "field {field_name:?} on event {:?} leaked the file-backend password: {value:?}",
                    event.name
                );
                assert!(
                    !value.contains(SECRET_STORED_VALUE),
                    "field {field_name:?} on event {:?} leaked the stored secret value: {value:?}",
                    event.name
                );
            }
            if let Some(message) = event.message() {
                assert!(!message.contains(&secret_needle_key));
                assert!(!message.contains(SECRET_PASSWORD));
                assert!(!message.contains(SECRET_STORED_VALUE));
            }
        }
    }
}
