use serde::{Deserialize, Serialize};
use tauri::command;

use elementium_types::{IceCandidate, SessionDescription};

/// Per-connection configuration from JS.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RtcConfiguration {
    #[allow(dead_code)]
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
pub struct PeerConnectionHandle {
    pub id: String,
}

#[command]
pub async fn create_peer_connection(
    config: Option<RtcConfiguration>,
) -> Result<PeerConnectionHandle, String> {
    let _ = config;
    // TODO: Create str0m Rtc instance, store in state map
    let id = uuid_v4();
    tracing::info!(pc_id = %id, "Creating peer connection");
    Ok(PeerConnectionHandle { id })
}

#[command]
pub async fn create_offer(pc_id: String) -> Result<SessionDescription, String> {
    tracing::info!(pc_id = %pc_id, "Creating offer");
    // TODO: Generate SDP offer via str0m
    Err("Not yet implemented".into())
}

#[command]
pub async fn create_answer(pc_id: String) -> Result<SessionDescription, String> {
    tracing::info!(pc_id = %pc_id, "Creating answer");
    // TODO: Generate SDP answer via str0m
    Err("Not yet implemented".into())
}

#[command]
pub async fn set_local_description(
    pc_id: String,
    description: SessionDescription,
) -> Result<(), String> {
    let _ = (pc_id, description);
    // TODO: Apply local SDP to str0m
    Err("Not yet implemented".into())
}

#[command]
pub async fn set_remote_description(
    pc_id: String,
    description: SessionDescription,
) -> Result<(), String> {
    let _ = (pc_id, description);
    // TODO: Apply remote SDP to str0m
    Err("Not yet implemented".into())
}

#[command]
pub async fn add_ice_candidate(pc_id: String, candidate: IceCandidate) -> Result<(), String> {
    let _ = (pc_id, candidate);
    // TODO: Feed ICE candidate to str0m
    Err("Not yet implemented".into())
}

#[command]
pub async fn close_peer_connection(pc_id: String) -> Result<(), String> {
    tracing::info!(pc_id = %pc_id, "Closing peer connection");
    // TODO: Tear down str0m Rtc instance
    Err("Not yet implemented".into())
}

fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("pc-{t:x}")
}
