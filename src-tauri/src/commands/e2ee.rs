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
use elementium_webrtc::EncryptionPolicy;
use elementium_webrtc::E2eeContext;

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

    let opts = E2eeOptions {
        ratchet_window_size: options
            .as_ref()
            .and_then(|o| o.ratchet_window_size)
            .unwrap_or(0),
        ratchet_salt: options.as_ref().and_then(|o| o.ratchet_salt.clone()),
        auto_ratchet: true,
    };

    let ctx = E2eeContext::new(opts);
    {
        let mut guard = state.ctx.lock_str()?;
        *guard = EncryptionPolicy::Encrypted(ctx);
    }

    tracing::info!("E2EE context initialized");
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
