use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State, command};

use elementium_types::{IceCandidate, SessionDescription};
use elementium_webrtc::engine::{IceServerConfig, WebRtcEngine};
use elementium_webrtc::peer_connection;

use super::media_devices::MediaState;

/// Shared WebRTC engine state, managed by Tauri.
#[derive(Clone)]
pub struct WebRtcState(pub Arc<Mutex<WebRtcEngine>>);

/// Per-connection configuration from JS.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct RtcConfiguration {
    pub ice_servers: Option<Vec<IceServer>>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

/// Result of creating a peer connection.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerConnectionResult {
    pub id: String,
}

/// Events emitted to the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum WebRtcEvent {
    #[serde(rename_all = "camelCase")]
    IceConnectionStateChange { pc_id: String, state: String },
    #[serde(rename_all = "camelCase")]
    ConnectionStateChange { pc_id: String, state: String },
    #[serde(rename_all = "camelCase")]
    IceCandidate { pc_id: String, candidate: String },
    #[serde(rename_all = "camelCase")]
    IceGatheringComplete { pc_id: String },
    #[serde(rename_all = "camelCase")]
    Connected { pc_id: String },
    #[serde(rename_all = "camelCase")]
    RemoteTrackAdded {
        pc_id: String,
        mid: String,
        kind: String,
    },
}

#[command]
pub async fn create_peer_connection(
    state: State<'_, WebRtcState>,
    app: AppHandle,
    config: Option<RtcConfiguration>,
) -> Result<PeerConnectionResult, String> {
    if let Some(ref cfg) = config {
        tracing::info!(?cfg, "ICE servers from signaling");
    }
    let id = generate_id();
    tracing::info!(pc_id = %id, "Creating peer connection");

    // Convert ICE server config to engine format
    let ice_servers: Option<Vec<IceServerConfig>> = config.as_ref().and_then(|cfg| {
        cfg.ice_servers.as_ref().map(|servers| {
            servers
                .iter()
                .map(|s| IceServerConfig {
                    urls: s.urls.clone(),
                    username: s.username.clone(),
                    credential: s.credential.clone(),
                })
                .collect()
        })
    });

    {
        let mut engine = state.0.lock().map_err(|e| e.to_string())?;
        engine.create_connection(id.clone(), ice_servers.as_deref())?;
    }

    // Spawn a task to forward events from the I/O loop to the frontend
    let state_clone: WebRtcState = state.inner().clone();
    let app_clone = app.clone();
    let id_clone = id.clone();
    tokio::spawn(async move {
        forward_events(&state_clone, &app_clone, &id_clone).await;
    });

    Ok(PeerConnectionResult { id })
}

/// Data channel info from JS.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsDataChannelInfo {
    pub label: String,
    pub ordered: Option<bool>,
    pub max_retransmits: Option<u16>,
    pub max_packet_life_time: Option<u16>,
    pub protocol: Option<String>,
}

/// Transceiver info from JS.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsTransceiverInfo {
    pub kind: String,
    pub direction: Option<String>,
}

#[command]
pub async fn create_offer(
    state: State<'_, WebRtcState>,
    media_state: State<'_, MediaState>,
    pc_id: String,
    include_video: Option<bool>,
    data_channels: Option<Vec<JsDataChannelInfo>>,
    transceivers: Option<Vec<JsTransceiverInfo>>,
) -> Result<SessionDescription, String> {
    let video = include_video.unwrap_or(false);
    tracing::info!(
        pc_id = %pc_id,
        include_video = video,
        num_dc = data_channels.as_ref().map(|d| d.len()).unwrap_or(0),
        num_tc = transceivers.as_ref().map(|t| t.len()).unwrap_or(0),
        "Creating offer"
    );

    let engine = state.0.lock().map_err(|e| e.to_string())?;
    let managed = engine.get(&pc_id).ok_or("Peer connection not found")?;

    // If video is included, connect the camera pipeline to this PC's I/O channel
    if video {
        let io_cmd_tx = managed.io_cmd_tx.clone();
        if let Ok(cam_guard) = media_state.camera.lock() {
            if let Some(ref cam) = *cam_guard {
                if let Ok(mut encode_guard) = cam.encode_tx.lock() {
                    tracing::info!(pc_id = %pc_id, "Connecting camera pipeline to peer connection");
                    *encode_guard = Some(io_cmd_tx);
                }
            }
        }
    }

    // Convert data channel info
    let dc_infos: Vec<peer_connection::DataChannelInfo> = data_channels
        .unwrap_or_default()
        .into_iter()
        .map(|dc| peer_connection::DataChannelInfo {
            label: dc.label,
            ordered: dc.ordered.unwrap_or(true),
            max_retransmits: dc.max_retransmits,
            max_packet_life_time: dc.max_packet_life_time,
            protocol: dc.protocol.unwrap_or_default(),
        })
        .collect();

    // Convert transceiver info
    let tc_infos: Vec<peer_connection::TransceiverInfo> = transceivers
        .unwrap_or_default()
        .into_iter()
        .map(|tc| peer_connection::TransceiverInfo::from_js(&tc.kind, tc.direction.as_deref()))
        .collect();

    let mut pc = managed.handle.lock().map_err(|e| e.to_string())?;
    peer_connection::create_offer(&mut pc, &dc_infos, &tc_infos)
}

#[command]
pub async fn create_answer(
    state: State<'_, WebRtcState>,
    pc_id: String,
) -> Result<SessionDescription, String> {
    tracing::info!(pc_id = %pc_id, "Creating answer");
    let engine = state.0.lock().map_err(|e| e.to_string())?;
    let managed = engine.get(&pc_id).ok_or("Peer connection not found")?;
    let mut pc = managed.handle.lock().map_err(|e| e.to_string())?;
    peer_connection::create_answer(&mut pc)
}

#[command]
pub async fn set_local_description(
    pc_id: String,
    description: SessionDescription,
) -> Result<(), String> {
    tracing::info!(pc_id = %pc_id, sdp_type = ?description.sdp_type, "Setting local description");
    let _ = (pc_id, description);
    Ok(())
}

#[command]
pub async fn set_remote_description(
    state: State<'_, WebRtcState>,
    pc_id: String,
    description: SessionDescription,
) -> Result<Option<SessionDescription>, String> {
    tracing::info!(pc_id = %pc_id, sdp_type = ?description.sdp_type, "Setting remote description");
    let engine = state.0.lock().map_err(|e| e.to_string())?;
    let managed = engine.get(&pc_id).ok_or("Peer connection not found")?;
    let mut pc = managed.handle.lock().map_err(|e| e.to_string())?;
    peer_connection::set_remote_description(&mut pc, &description)
}

#[command]
pub async fn add_ice_candidate(
    state: State<'_, WebRtcState>,
    pc_id: String,
    candidate: IceCandidate,
) -> Result<(), String> {
    tracing::info!(pc_id = %pc_id, candidate = %candidate.candidate, "Adding ICE candidate");
    let engine = state.0.lock().map_err(|e| e.to_string())?;
    let managed = engine.get(&pc_id).ok_or("Peer connection not found")?;
    let mut pc = managed.handle.lock().map_err(|e| e.to_string())?;
    peer_connection::add_ice_candidate(&mut pc, &candidate.candidate)
}

#[command]
pub async fn close_peer_connection(
    state: State<'_, WebRtcState>,
    pc_id: String,
) -> Result<(), String> {
    tracing::info!(pc_id = %pc_id, "Closing peer connection");
    let mut engine = state.0.lock().map_err(|e| e.to_string())?;
    engine.remove(&pc_id);
    Ok(())
}

/// Forward events from the I/O loop to the Tauri frontend via webview.eval().
///
/// Uses eval() instead of the Tauri event system (emit/listen) because
/// Tauri's permission check blocks listen() from non-local URLs
/// (e.g. http://localhost:5173 in dev mode). eval() bypasses this entirely.
async fn forward_events(state: &WebRtcState, app: &AppHandle, pc_id: &str) {
    loop {
        let event = {
            let engine = match state.0.lock() {
                Ok(e) => e,
                Err(_) => return,
            };
            let managed = match engine.get(pc_id) {
                Some(m) => m,
                None => return,
            };
            let mut rx = match managed.event_rx.lock() {
                Ok(rx) => rx,
                Err(_) => return,
            };
            rx.try_recv().ok()
        };

        match event {
            Some(pc_event) => {
                let tauri_event = match pc_event {
                    elementium_webrtc::PcEvent::IceConnectionStateChange(s) => {
                        WebRtcEvent::IceConnectionStateChange {
                            pc_id: pc_id.to_string(),
                            state: format!("{s:?}").to_lowercase(),
                        }
                    }
                    elementium_webrtc::PcEvent::ConnectionStateChange(s) => {
                        WebRtcEvent::ConnectionStateChange {
                            pc_id: pc_id.to_string(),
                            state: format!("{s:?}").to_lowercase(),
                        }
                    }
                    elementium_webrtc::PcEvent::IceCandidate(candidate) => {
                        WebRtcEvent::IceCandidate {
                            pc_id: pc_id.to_string(),
                            candidate,
                        }
                    }
                    elementium_webrtc::PcEvent::IceGatheringComplete => {
                        WebRtcEvent::IceGatheringComplete {
                            pc_id: pc_id.to_string(),
                        }
                    }
                    elementium_webrtc::PcEvent::Connected => WebRtcEvent::Connected {
                        pc_id: pc_id.to_string(),
                    },
                    elementium_webrtc::PcEvent::RemoteTrackAdded { mid, kind } => {
                        WebRtcEvent::RemoteTrackAdded {
                            pc_id: pc_id.to_string(),
                            mid,
                            kind,
                        }
                    }
                    elementium_webrtc::PcEvent::AudioData(_) => {
                        // Audio data is handled by the audio playback pipeline
                        continue;
                    }
                    elementium_webrtc::PcEvent::VideoData(_) => {
                        // Video data is handled by the video playback pipeline
                        continue;
                    }
                };

                // Push event to JS via eval() — calls the global handler registered
                // by the WebRTC shim on window.top
                if let Some(webview) = app.get_webview_window("main") {
                    if let Ok(json) = serde_json::to_string(&tauri_event) {
                        let js = format!(
                            "if(window.__elementium_webrtc_event)window.__elementium_webrtc_event({})",
                            json
                        );
                        match webview.eval(&js) {
                            Ok(_) => tracing::debug!(pc_id = pc_id, "eval sent to JS"),
                            Err(e) => tracing::error!(pc_id = pc_id, err = %e, "eval failed"),
                        }
                    }
                } else {
                    tracing::error!("No 'main' webview found for eval");
                }
            }
            None => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
    }
}

fn generate_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("pc-{t:x}")
}
