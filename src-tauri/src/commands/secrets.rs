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

use elementium_keyring::{BackendType, SecretStore, file_backend::FileBackend};

/// Managed state for the secret store.
pub struct SecretStoreState {
    pub store: Arc<Mutex<Option<Box<dyn SecretStore>>>>,
    pub backend_type: Arc<Mutex<BackendType>>,
}

#[command]
pub async fn secret_get(
    key: String,
    state: State<'_, SecretStoreState>,
) -> Result<Option<String>, String> {
    let store = state.store.clone();
    tokio::task::spawn_blocking(move || {
        let guard = store.lock().map_err(|e| e.to_string())?;
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
    let store = state.store.clone();
    tokio::task::spawn_blocking(move || {
        let guard = store.lock().map_err(|e| e.to_string())?;
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
    let store = state.store.clone();
    tokio::task::spawn_blocking(move || {
        let guard = store.lock().map_err(|e| e.to_string())?;
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
    let store = state.store.clone();
    tokio::task::spawn_blocking(move || {
        let guard = store.lock().map_err(|e| e.to_string())?;
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
    let guard = state.backend_type.lock().map_err(|e| e.to_string())?;
    Ok(*guard)
}

#[command]
pub async fn secret_setup_file_backend(
    password: String,
    state: State<'_, SecretStoreState>,
) -> Result<(), String> {
    let store_arc = state.store.clone();
    let backend_type_arc = state.backend_type.clone();

    tokio::task::spawn_blocking(move || {
        let backend = FileBackend::new(&password).map_err(|e| e.to_string())?;
        let boxed: Box<dyn SecretStore> = Box::new(backend);

        {
            let mut store_guard = store_arc.lock().map_err(|e| e.to_string())?;
            *store_guard = Some(boxed);
        }

        {
            let mut bt_guard = backend_type_arc.lock().map_err(|e| e.to_string())?;
            *bt_guard = BackendType::EncryptedFile;
        }

        Ok(())
    })
    .await
    .map_err(|e: tokio::task::JoinError| e.to_string())?
}
