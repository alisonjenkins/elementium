use tauri::command;

use elementium_types::{CaptureSource, TrackId};

#[command]
pub async fn get_capture_sources() -> Result<Vec<CaptureSource>, String> {
    tracing::info!("Getting available capture sources");
    // TODO: Use elementium-screen to enumerate monitors/windows
    Ok(vec![])
}

#[command]
pub async fn get_display_media(source_id: String) -> Result<TrackId, String> {
    tracing::info!(source_id = %source_id, "Starting screen capture");
    // TODO: Start screen capture for the selected source
    Err("Not yet implemented".into())
}
