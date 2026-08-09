// Every `#[tauri::command]` async fn below that takes a `State<'_, T>` parameter causes
// the `#[command]` macro to generate a sibling IPC-dispatch wrapper item in this module
// containing an internal match with an arm clippy flags as unreachable. That wrapper is
// framework codegen (not nested inside the fn item itself, so a function- or
// statement-scoped `#[allow]` cannot reach it — verified empirically), hence the
// module-level allow here rather than the usual per-item scoping.
#![allow(clippy::unreachable)]
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use tauri::{State, command};

use elementium_e2ee::E2eeOptions;
use elementium_webrtc::E2eeContext;
use elementium_webrtc::EncryptionPolicy;

use super::LockExt;

/// Shared E2EE state, managed by Tauri.
///
/// Also the connection-encryption policy consumed by the WebRTC engine and `LiveKit`
/// transport (see `main.rs`'s `register_state`): "not yet initialized" and "this
/// connection is deliberately unencrypted" are the same state here, both represented
/// by [`EncryptionPolicy::ExplicitlyUnencrypted`] rather than by an absent `Option`.
#[derive(Clone)]
pub struct E2eeState {
    pub ctx: Arc<Mutex<EncryptionPolicy>>,
}

/// Options received from the JS E2EE Worker's init message.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct JsE2eeOptions {
    pub ratchet_window_size: Option<u32>,
    pub ratchet_salt: Option<String>,
    pub failure_tolerance: Option<i32>,
}

#[command]
pub async fn e2ee_init(
    state: State<'_, E2eeState>,
    options: Option<JsE2eeOptions>,
) -> Result<(), String> {
    tracing::info!("E2EE init requested");

    let ratchet_window_size = options
        .as_ref()
        .and_then(|o| o.ratchet_window_size)
        .unwrap_or(0);
    let opts = E2eeOptions {
        ratchet_window_size: options
            .as_ref()
            .and_then(|o| o.ratchet_window_size)
            .unwrap_or(0),
        ratchet_salt: options.as_ref().and_then(|o| o.ratchet_salt.clone()),
        auto_ratchet: true,
    };

    // A repeat init updates the options and keeps the keys.
    //
    // Replacing the context wholesale discarded every key installed so far, and inits do
    // repeat: livekit sends one per key provider, and the bridge synthesises another
    // whenever a key arrives before the real one. In a real call that sequence ran
    // key, key, key, init -- and the init threw all three away.
    let mut guard = state.ctx.lock_str()?;
    if let EncryptionPolicy::Encrypted(existing) = &*guard {
        existing.set_options(opts);
        tracing::info!(
            ratchet_window_size,
            "E2EE re-initialised: options updated, keys kept"
        );
    } else {
        *guard = EncryptionPolicy::Encrypted(E2eeContext::new(opts));
        tracing::info!(ratchet_window_size, "E2EE context initialized");
    }
    drop(guard);
    Ok(())
}

#[command]
pub async fn e2ee_set_key(
    state: State<'_, E2eeState>,
    participant: String,
    key_index: u8,
    key_material: Vec<u8>,
) -> Result<(), String> {
    tracing::info!(
        participant = %participant,
        key_index = key_index,
        key_len = key_material.len(),
        "E2EE key received"
    );

    state
        .ctx
        .lock_str()?
        .as_context()
        .ok_or("E2EE not initialized — call e2ee_init first")?
        .set_key(&participant, key_index, &key_material);
    Ok(())
}

/// Record the SFU's server-injected-frame marker.
///
/// Frames ending with it are the SFU's own -- silence or a black picture while a publisher
/// is muted -- and carry no encryption, because the SFU holds no key. Without this every
/// one is fed to AES-GCM, fails to authenticate and is dropped, which in a log is
/// indistinguishable from a missing key.
///
/// # Errors
///
/// Returns an error if E2EE has not been initialised or the context lock is poisoned.
#[command]
pub async fn e2ee_set_sif_trailer(
    state: State<'_, E2eeState>,
    trailer: Vec<u8>,
) -> Result<(), String> {
    tracing::info!(
        len = trailer.len(),
        "E2EE server-injected-frame trailer received"
    );

    state
        .ctx
        .lock_str()?
        .as_context()
        .ok_or("E2EE not initialized — call e2ee_init first")?
        .set_sif_trailer(trailer)
        .map_err(|e| e.to_string())
}

#[command]
pub async fn e2ee_set_local_identity(
    state: State<'_, E2eeState>,
    identity: String,
) -> Result<(), String> {
    tracing::info!(identity = %identity, "E2EE local identity set");

    state
        .ctx
        .lock_str()?
        .as_context()
        .ok_or("E2EE not initialized — call e2ee_init first")?
        .set_local_identity(&identity);
    Ok(())
}
