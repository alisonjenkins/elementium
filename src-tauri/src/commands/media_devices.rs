use tauri::command;

use elementium_types::{MediaConstraints, MediaDevice, TrackId};

#[command]
pub async fn enumerate_devices() -> Result<Vec<MediaDevice>, String> {
    // TODO: Use cpal + nokhwa to enumerate real devices
    tracing::info!("Enumerating media devices");
    Ok(vec![])
}

#[command]
pub async fn get_user_media(constraints: MediaConstraints) -> Result<Vec<TrackId>, String> {
    tracing::info!(?constraints, "getUserMedia request");
    // TODO: Start audio/video capture based on constraints, return track IDs
    Err("Not yet implemented".into())
}

#[command]
pub async fn stop_track(track_id: TrackId) -> Result<(), String> {
    tracing::info!(%track_id, "Stopping track");
    // TODO: Stop the capture associated with this track
    Err("Not yet implemented".into())
}
