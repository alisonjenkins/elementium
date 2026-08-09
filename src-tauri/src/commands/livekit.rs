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
use tauri::{AppHandle, Emitter, Manager, State, command};
use tracing::Instrument;

use elementium_types::{CorrelationId, MediaTrackKey};
use elementium_webrtc::engine::VideoFrameBuffer;
use elementium_webrtc::livekit::room::{LiveKitRoom, RoomEvent};

use super::LockExt;
use super::e2ee::E2eeState;
use super::media_devices::MediaState;

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
    media_state: State<'_, MediaState>,
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
            // Recorded so capture logs under the same id as the session that will carry
            // their frames. Set here rather than at the room's creation because this is the
            // point the room is known to be usable, and a capture pipeline started against
            // a failed connection should not claim its id.
            if let Ok(mut slot) = media_state.session_correlation.lock() {
                *slot = Some(correlation_id.clone());
            }
            let mut rooms = state.rooms.lock_str()?;
            rooms.insert(room_id.clone(), room);
        }

        // Spawn event forwarder, instrumented with the session span so its
        // events keep the same correlation_id.
        let app_clone = app.clone();
        let forwarder_span = tracing::Span::current();
        // Resolved from the app handle rather than taken as a parameter: the forwarder
        // outlives this call, so it cannot borrow a `State` guard.
        let app_for_codec = app.clone();
        tokio::spawn(
            async move {
                while let Some(event) = event_rx.recv().await {
                    // Acted on, not only forwarded. The SFU is the only party that knows
                    // who is subscribed, so when it says the room has moved to a codec the
                    // encoder must follow -- otherwise a participant who cannot decode our
                    // stream simply sees nothing, with nothing to explain it.
                    if let RoomEvent::PublishCodecChanged { codec, .. } = &event {
                        apply_publish_codec(&app_for_codec.state::<MediaState>(), codec);
                    }
                    let event_name = match &event {
                        RoomEvent::ParticipantJoined { .. } => "livekit-participant-joined",
                        RoomEvent::ParticipantLeft { .. } => "livekit-participant-left",
                        RoomEvent::TrackSubscribed { .. } => "livekit-track-subscribed",
                        RoomEvent::TrackUnsubscribed { .. } => "livekit-track-unsubscribed",
                        RoomEvent::ConnectionStateChanged { .. } => "livekit-connection-state",
                        RoomEvent::ActiveSpeakersChanged { .. } => "livekit-active-speakers",
                        RoomEvent::PublishCodecChanged { .. } => "livekit-publish-codec-changed",
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
    media_state: State<'_, MediaState>,
    room_id: String,
    kind: String,
    source: String,
) -> Result<(), String> {
    let key = track_key(&kind, &source)?;
    let room = get_room(&state, &room_id)?;
    let media_tx = {
        let mut room = room.lock().await;
        room.publish_track(key)?;
        room.media_sender()
    };
    attach_capture_pipeline(&media_state, media_tx, key);
    Ok(())
}

/// Tell the SFU that a published track is muted, or unmuted.
///
/// The page stopping or disabling a track is a local fact until someone says so on the
/// wire. Without this, every other participant's UI keeps showing the track live, and what
/// they see is a camera tile frozen on its last frame — which looks exactly like a network
/// stall rather than a person switching their camera off.
///
/// # Errors
///
/// Returns an error if the room is unknown, the kind/source pair is not one we publish, or
/// the track was never published — muting something that was never announced cannot be
/// reported as success, because the caller would take it as done.
#[command]
pub async fn livekit_set_track_muted(
    state: State<'_, LiveKitState>,
    room_id: String,
    kind: String,
    source: String,
    muted: bool,
) -> Result<(), String> {
    let key = track_key(&kind, &source)?;
    let room = get_room(&state, &room_id)?;
    let room = room.lock().await;
    room.set_track_muted(key, muted).map_err(|e| e.to_string())
}

/// Resolve the kind/source pair the frontend sends into a track identity.
///
/// Rejected rather than defaulted when the pair is not one we publish. The old code mapped
/// an unrecognised source to `TrackSource::Unknown` and carried on, which produces a track
/// the SFU accepts and every other client renders as an unlabelled stream -- a failure that
/// only shows up in somebody else's UI.
pub(super) fn track_key(kind: &str, source: &str) -> Result<MediaTrackKey, String> {
    let key = match (kind, source) {
        ("audio", "microphone") => MediaTrackKey::microphone(),
        ("video", "camera") => MediaTrackKey::camera(),
        ("video", "screen_share") => MediaTrackKey::screen_share(),
        ("audio", "screen_share_audio") => MediaTrackKey::screen_share_audio(),
        _ => return Err(format!("not a publishable track: {kind}/{source}")),
    };
    debug_assert_eq!(key.kind().as_str(), kind);
    debug_assert_eq!(key.source().as_str(), source);
    Ok(key)
}

/// Tell the capture pipeline which codec the SFU wants from now on.
///
/// The pipeline rebuilds its encoder on the next frame. Nothing is renegotiated and the
/// track is not restarted: a late participant who cannot decode the current codec must not
/// cost everyone else an interruption.
fn apply_publish_codec(media_state: &MediaState, codec: &str) {
    let Some(codec) = elementium_codec::VideoCodec::from_mime(codec) else {
        tracing::warn!(
            codec,
            "SFU named a codec we do not recognise; leaving the encoder alone"
        );
        return;
    };
    let Ok(pipelines) = media_state.pipelines.lock() else {
        return;
    };
    // Applied to every video pipeline: the SFU's codec preference is a property of the
    // room, so a camera and a screen share published into it are subject to the same
    // answer, and leaving one on the old codec would publish a track nobody can decode.
    let mut changed = 0_usize;
    for handle in pipelines.values() {
        let Some((_, active_codec)) = handle.video() else {
            continue;
        };
        if active_codec.get() == codec {
            continue;
        }
        active_codec.set(codec);
        changed = changed.saturating_add(1);
        tracing::info!(
            codec = codec.sdp_name(),
            key = %handle.key,
            "switching publish codec at the SFU's request"
        );
    }
    if changed == 0 {
        tracing::debug!(
            codec = codec.sdp_name(),
            "codec change with no video pipeline to apply it to"
        );
    }
}

/// Point the running capture pipeline for `kind` at this room, so its encoded frames
/// actually leave the machine.
///
/// A pipeline that is capturing but unattached is silent in exactly the way that is
/// hardest to notice: the camera light is on, the local preview updates, the encoder logs
/// frames, and the far end sees nothing. Logging both outcomes -- attached, and "no
/// pipeline running" -- is deliberate, because "publish succeeded" alone cannot
/// distinguish them.
fn attach_capture_pipeline(
    media_state: &MediaState,
    media_tx: tokio::sync::mpsc::Sender<elementium_webrtc::engine::IoCommand>,
    key: MediaTrackKey,
) {
    let kind = key.kind().as_str();
    let connection = media_state
        .pipelines
        .lock()
        .ok()
        .and_then(|guard| guard.get(&key).map(|h| h.encode_tx.clone()));

    // Remembered for pipelines that have not started yet: joining a call muted publishes
    // the microphone track long before `getUserMedia` opens the device.
    if let Ok(mut guard) = media_state.sfu_media_tx.lock() {
        *guard = Some(media_tx.clone());
    }

    let Some(connection) = connection else {
        tracing::warn!(
            kind,
            %key,
            "published a track with no capture pipeline running; nothing will be sent \
             until the pipeline starts and is attached"
        );
        return;
    };

    if let Ok(mut guard) = connection.lock() {
        *guard = Some(media_tx);
        tracing::info!(kind, %key, "capture pipeline attached to the SFU room");
    }
}

/// Disconnect from a `LiveKit` room.
#[command]
pub async fn livekit_disconnect(
    state: State<'_, LiveKitState>,
    media_state: State<'_, MediaState>,
    room_id: String,
) -> Result<(), String> {
    // Detach the capture pipelines first: a pipeline left pointing at a torn-down room
    // keeps encoding into a dead channel, and a pipeline that outlives the call would
    // otherwise be handed to the *next* room by inheritance.
    detach_capture_pipelines(&media_state);

    let room = {
        let mut rooms = state.rooms.lock_str()?;
        rooms.remove(&room_id)
    };

    if let Some(room) = room {
        let mut room = room.lock().await;
        let span = tracing::info_span!("session", correlation_id = %room.correlation_id(), room_id = %room_id);
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

/// Forget the room the capture pipelines were feeding, leaving them capturing but
/// unpublished.
fn detach_capture_pipelines(media_state: &MediaState) {
    // The call's id goes with the call. A pipeline that outlives the room -- a preview left
    // running after hanging up -- should not keep logging under a session that has ended,
    // which would make a later log read as though the call were still up.
    if let Ok(mut slot) = media_state.session_correlation.lock() {
        *slot = None;
    }
    if let Ok(mut guard) = media_state.sfu_media_tx.lock() {
        *guard = None;
    }
    let Ok(pipelines) = media_state.pipelines.lock() else {
        return;
    };
    for handle in pipelines.values() {
        if let Ok(mut guard) = handle.encode_tx.lock() {
            *guard = None;
        }
    }
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

