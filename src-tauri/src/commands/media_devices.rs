use std::sync::Mutex;

use tauri::{State, command};

use elementium_media::device_enumeration;
use elementium_types::{MediaConstraints, MediaDevice, TrackId};

use super::webrtc::WebRtcState;

/// State for active media tracks (audio capture, video capture, etc.).
pub struct MediaState {
    pub active_tracks: Mutex<Vec<TrackId>>,
}

#[command]
pub async fn enumerate_devices() -> Result<Vec<MediaDevice>, String> {
    tracing::info!("Enumerating media devices");

    let mut devices = device_enumeration::enumerate_audio_devices();

    // Add video input devices
    // nokhwa device enumeration is best-effort
    if let Ok(cameras) = nokhwa::query(nokhwa::utils::ApiBackend::Auto) {
        for (i, cam) in cameras.iter().enumerate() {
            devices.push(MediaDevice {
                id: format!("video-input-{i}"),
                label: cam.human_name().to_string(),
                kind: elementium_types::MediaDeviceKind::VideoInput,
            });
        }
    }

    Ok(devices)
}

#[command]
pub async fn get_user_media(
    _webrtc_state: State<'_, WebRtcState>,
    media_state: State<'_, MediaState>,
    constraints: MediaConstraints,
) -> Result<Vec<TrackId>, String> {
    tracing::info!(?constraints, "getUserMedia request");
    let mut track_ids = Vec::new();

    if constraints.audio.is_some() {
        let track_id = TrackId(format!("audio-{}", generate_track_id()));
        tracing::info!(track_id = %track_id, "Starting audio capture");

        if let Ok(mut tracks) = media_state.active_tracks.lock() {
            tracks.push(track_id.clone());
        }
        track_ids.push(track_id);
    }

    if constraints.video.is_some() {
        let track_id = TrackId(format!("video-{}", generate_track_id()));
        tracing::info!(track_id = %track_id, "Starting video capture");

        if let Ok(mut tracks) = media_state.active_tracks.lock() {
            tracks.push(track_id.clone());
        }
        track_ids.push(track_id);
    }

    Ok(track_ids)
}

#[command]
pub async fn stop_track(
    media_state: State<'_, MediaState>,
    track_id: TrackId,
) -> Result<(), String> {
    tracing::info!(%track_id, "Stopping track");
    if let Ok(mut tracks) = media_state.active_tracks.lock() {
        tracks.retain(|t| t != &track_id);
    }
    Ok(())
}

fn generate_track_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{t:x}")
}
