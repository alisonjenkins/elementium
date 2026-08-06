// Every `#[tauri::command]` async fn below that takes a `State<'_, T>` parameter causes
// the `#[command]` macro to generate a sibling IPC-dispatch wrapper item in this module
// containing an internal match with an arm clippy flags as unreachable. That wrapper is
// framework codegen (not nested inside the fn item itself, so a function- or
// statement-scoped `#[allow]` cannot reach it — verified empirically), hence the
// module-level allow here rather than the usual per-item scoping.
#![allow(clippy::unreachable)]
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, State, command};
use tracing::Instrument;

use elementium_types::CorrelationId;
use elementium_webrtc::engine::VideoFrameBuffer;
use elementium_webrtc::livekit::room::{LiveKitRoom, RoomEvent};

use super::LockExt;
use super::e2ee::E2eeState;

/// Shared state holding active `LiveKit` rooms, managed by Tauri.
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

/// Connect to a `LiveKit` SFU room.
#[command]
pub async fn livekit_connect(
    state: State<'_, LiveKitState>,
    e2ee_state: State<'_, E2eeState>,
    app: AppHandle,
    sfu_url: String,
    token: String,
) -> Result<ConnectResult, String> {
    let correlation_id = CorrelationId::new();
    let span = tracing::info_span!("session", correlation_id = %correlation_id);

    // Same shared E2EE policy the direct-WebRTC path uses (see main.rs's
    // register_state doc comment): read the current policy at connect time.
    // EncryptionPolicy clones cheaply (Arc-backed E2eeContext inside), so
    // later e2ee_set_key calls remain visible through this clone.
    let e2ee = e2ee_state.ctx.lock_str()?.clone();

    async move {
        tracing::info!(sfu_url = %sfu_url, "connect attempt started");

        let video_frames = state.video_frames.clone();
        let connect_result =
            LiveKitRoom::connect(&sfu_url, &token, video_frames, correlation_id.clone(), e2ee)
                .await;
        let (room, mut event_rx) = match connect_result {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(reason = %e, "connect attempt failed");
                return Err(e.into());
            }
        };

        let room_id = room.room_id.clone();
        let room_name = room.room_name.clone();
        let local_identity = room.local_identity.clone();

        tracing::info!(
            room_id = %room_id,
            room_name = %room_name,
            local_identity = %local_identity,
            "connected"
        );

        let room = Arc::new(tokio::sync::Mutex::new(room));

        // Store in state
        {
            let mut rooms = state.rooms.lock_str()?;
            rooms.insert(room_id.clone(), room);
        }

        // Spawn event forwarder, instrumented with the session span so its
        // events keep the same correlation_id.
        let app_clone = app.clone();
        let forwarder_span = tracing::Span::current();
        tokio::spawn(
            async move {
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
            }
            .instrument(forwarder_span),
        );

        Ok(ConnectResult {
            room_id,
            room_name,
            local_identity,
        })
    }
    .instrument(span)
    .await
}

/// Publish a local track (audio/video) to the `LiveKit` room.
#[command]
pub async fn livekit_publish_track(
    state: State<'_, LiveKitState>,
    room_id: String,
    kind: String,
    source: String,
) -> Result<(), String> {
    let room = get_room(&state, &room_id)?;
    let mut room = room.lock().await;
    Ok(room.publish_track(&kind, &source)?)
}

/// Disconnect from a `LiveKit` room.
#[command]
pub async fn livekit_disconnect(
    state: State<'_, LiveKitState>,
    room_id: String,
) -> Result<(), String> {
    let room = {
        let mut rooms = state.rooms.lock_str()?;
        rooms.remove(&room_id)
    };

    if let Some(room) = room {
        let mut room = room.lock().await;
        let span =
            tracing::info_span!("session", correlation_id = %room.correlation_id(), room_id = %room_id);
        async {
            tracing::info!("disconnect requested");
            room.disconnect().await;
        }
        .instrument(span)
        .await;
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
    let rooms = state.rooms.lock_str()?;
    rooms
        .get(room_id)
        .cloned()
        .ok_or_else(|| format!("Room not found: {room_id}"))
}
