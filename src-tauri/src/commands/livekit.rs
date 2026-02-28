use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, State, command};

use elementium_webrtc::engine::VideoFrameBuffer;
use elementium_webrtc::livekit::room::{LiveKitRoom, RoomEvent};

/// Shared state holding active LiveKit rooms, managed by Tauri.
#[derive(Clone)]
pub struct LiveKitState {
    pub rooms: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<LiveKitRoom>>>>>,
    pub video_frames: VideoFrameBuffer,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectResult {
    pub room_id: String,
    pub room_name: String,
    pub local_identity: String,
}

/// Connect to a LiveKit SFU room.
#[command]
pub async fn livekit_connect(
    state: State<'_, LiveKitState>,
    app: AppHandle,
    sfu_url: String,
    token: String,
) -> Result<ConnectResult, String> {
    tracing::info!(sfu_url = %sfu_url, "Connecting to LiveKit room");

    let video_frames = state.video_frames.clone();
    let (room, mut event_rx) =
        LiveKitRoom::connect(&sfu_url, &token, video_frames).await?;

    let room_id = room.room_id.clone();
    let room_name = room.room_name.clone();
    let local_identity = room.local_identity.clone();

    let room = Arc::new(tokio::sync::Mutex::new(room));

    // Store in state
    {
        let mut rooms = state.rooms.lock().map_err(|e| e.to_string())?;
        rooms.insert(room_id.clone(), room);
    }

    // Spawn event forwarder
    let app_clone = app.clone();
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            let event_name = match &event {
                RoomEvent::ParticipantJoined { .. } => "livekit-participant-joined",
                RoomEvent::ParticipantLeft { .. } => "livekit-participant-left",
                RoomEvent::TrackSubscribed { .. } => "livekit-track-subscribed",
                RoomEvent::TrackUnsubscribed { .. } => "livekit-track-unsubscribed",
                RoomEvent::ConnectionStateChanged { .. } => "livekit-connection-state",
                RoomEvent::ActiveSpeakersChanged { .. } => "livekit-active-speakers",
            };
            let _ = app_clone.emit(event_name, &event);
        }
        tracing::info!("LiveKit event forwarder ended");
    });

    Ok(ConnectResult {
        room_id,
        room_name,
        local_identity,
    })
}

/// Publish a local track (audio/video) to the LiveKit room.
#[command]
pub async fn livekit_publish_track(
    state: State<'_, LiveKitState>,
    room_id: String,
    kind: String,
    source: String,
) -> Result<(), String> {
    let room = get_room(&state, &room_id)?;
    let mut room = room.lock().await;
    room.publish_track(&kind, &source)
}

/// Disconnect from a LiveKit room.
#[command]
pub async fn livekit_disconnect(
    state: State<'_, LiveKitState>,
    room_id: String,
) -> Result<(), String> {
    tracing::info!(room_id = %room_id, "Disconnecting from LiveKit room");

    let room = {
        let mut rooms = state.rooms.lock().map_err(|e| e.to_string())?;
        rooms.remove(&room_id)
    };

    if let Some(room) = room {
        let mut room = room.lock().await;
        room.disconnect().await;
    }

    Ok(())
}

/// Set subscriber volume for a participant (0.0 to 1.0).
#[command]
pub async fn livekit_set_subscriber_volume(
    _state: State<'_, LiveKitState>,
    _room_id: String,
    _participant_id: String,
    _volume: f32,
) -> Result<(), String> {
    // TODO: Per-participant volume control requires mixing with per-source gains
    tracing::info!("livekit_set_subscriber_volume not yet implemented");
    Ok(())
}

fn get_room(
    state: &LiveKitState,
    room_id: &str,
) -> Result<Arc<tokio::sync::Mutex<LiveKitRoom>>, String> {
    let rooms = state.rooms.lock().map_err(|e| e.to_string())?;
    rooms
        .get(room_id)
        .cloned()
        .ok_or_else(|| format!("Room not found: {room_id}"))
}
