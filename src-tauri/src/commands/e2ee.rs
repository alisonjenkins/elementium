use std::sync::{Arc, Mutex};

use serde::Deserialize;
use tauri::{State, command};

use elementium_e2ee::{E2eeContext, E2eeOptions};

/// Shared E2EE state, managed by Tauri.
#[derive(Clone)]
pub struct E2eeState {
    pub ctx: Arc<Mutex<Option<E2eeContext>>>,
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
    let mut guard = state.ctx.lock().map_err(|e| e.to_string())?;
    *guard = Some(ctx);

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
    let guard = state.ctx.lock().map_err(|e| e.to_string())?;
    let ctx = guard
        .as_ref()
        .ok_or("E2EE not initialized — call e2ee_init first")?;

    tracing::info!(
        participant = %participant,
        key_index = key_index,
        key_len = key_material.len(),
        "E2EE key received"
    );

    ctx.set_key(&participant, key_index, &key_material);
    Ok(())
}

#[command]
pub async fn e2ee_set_local_identity(
    state: State<'_, E2eeState>,
    identity: String,
) -> Result<(), String> {
    let guard = state.ctx.lock().map_err(|e| e.to_string())?;
    let ctx = guard
        .as_ref()
        .ok_or("E2EE not initialized — call e2ee_init first")?;

    tracing::info!(identity = %identity, "E2EE local identity set");
    ctx.set_local_identity(&identity);
    Ok(())
}
