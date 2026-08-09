// Every `#[tauri::command]` async fn below that takes a `State<'_, T>` parameter causes
// the `#[command]` macro to generate a sibling IPC-dispatch wrapper item in this module
// containing an internal match with an arm clippy flags as unreachable. That wrapper is
// framework codegen (not nested inside the fn item itself, so a function- or
// statement-scoped `#[allow]` cannot reach it — verified empirically), hence the
// module-level allow here rather than the usual per-item scoping.
#![allow(clippy::unreachable)]
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State, command};
use tokio::sync::mpsc as tokio_mpsc;
use tracing::Instrument;

use elementium_types::{CorrelationId, IceCandidate, SdpType, SessionDescription};
use elementium_webrtc::engine::{IceServerConfig, WebRtcEngine};
use elementium_webrtc::peer_connection;
use elementium_webrtc::{PcEvent, VideoPipeline, start_audio_playback};

use super::media_devices::MediaState;
use super::{IpcErr, IpcErrCoded, LockExt};

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
        /// The `MediaStream` id from the remote SDP. livekit-client routes an arriving track
        /// by this and nothing else -- see `PcEvent::RemoteTrackAdded`.
        stream_id: Option<String>,
    },
    /// A data channel's DCEP handshake completed; the shim's `RTCDataChannel` for `label`
    /// should flip to `readyState: "open"` and fire `onopen`.
    #[serde(rename_all = "camelCase")]
    DataChannelOpen { pc_id: String, label: String },
    /// A message arrived on the data channel named `label`. `binary` distinguishes the DOM
    /// `ArrayBuffer` case from the string case for the shim's `onmessage` to reconstruct;
    /// `data` is the raw bytes either way.
    #[serde(rename_all = "camelCase")]
    DataChannelMessage {
        pc_id: String,
        label: String,
        binary: bool,
        data: Vec<u8>,
    },
    /// The data channel named `label` closed, locally or by the remote peer.
    #[serde(rename_all = "camelCase")]
    DataChannelClose { pc_id: String, label: String },
}

/// Convert one Rust-side [`peer_connection::DataChannelEvent`] into the JSON shape the
/// shim expects, tagging it with the connection it came from.
///
/// Split out of the forwarding loop because it has nothing to do with polling or locking
/// -- it is pure data massaging, and keeping it separate is what makes that loop's own
/// logic (which fires only when there is something new) readable on its own.
fn data_channel_event_to_webrtc(
    pc_id: &str,
    event: peer_connection::DataChannelEvent,
) -> WebRtcEvent {
    match event {
        peer_connection::DataChannelEvent::Open { label } => WebRtcEvent::DataChannelOpen {
            pc_id: pc_id.to_string(),
            label,
        },
        peer_connection::DataChannelEvent::Message {
            label,
            binary,
            data,
        } => WebRtcEvent::DataChannelMessage {
            pc_id: pc_id.to_string(),
            label,
            binary,
            data,
        },
        peer_connection::DataChannelEvent::Close { label } => WebRtcEvent::DataChannelClose {
            pc_id: pc_id.to_string(),
            label,
        },
    }
}

#[command]
pub async fn create_peer_connection(
    state: State<'_, WebRtcState>,
    media_state: State<'_, MediaState>,
    app: AppHandle,
    config: Option<RtcConfiguration>,
) -> Result<PeerConnectionResult, String> {
    let id = generate_id();
    let correlation_id = CorrelationId::new();
    let span = tracing::info_span!(
        "peer_connection",
        correlation_id = %correlation_id,
        pc_id = %id
    );
    let _enter = span.enter();

    if let Some(ref cfg) = config {
        // Counted and named by URL, never printed whole. `RtcConfiguration`'s derived
        // `Debug` includes each server's `username` and `credential`, which for TURN are
        // live credentials the SFU just issued -- exactly the class of leak the full-SDP
        // logging turned out to be. Unexercised in the logs seen so far only because this
        // deployment sends no ICE servers; the first one that does would have written them
        // to disk.
        let servers = cfg.ice_servers.as_deref().unwrap_or_default();
        tracing::info!(
            count = servers.len(),
            urls = ?servers.iter().flat_map(|s| s.urls.iter()).collect::<Vec<_>>(),
            with_credentials = servers.iter().filter(|s| s.credential.is_some()).count(),
            "ICE servers from signaling"
        );
    }
    tracing::info!(pc_id = %id, "peer connection created");

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
        let mut engine = state.0.lock_str("create_peer_connection")?;
        engine
            .create_connection(id.clone(), ice_servers.as_deref())
            .ipc_err("create_peer_connection", &id)?;
    }

    // Adopt any capture pipeline that is currently attached to nothing.
    //
    // `create_offer` is otherwise the only place a pipeline is attached, which leaves a
    // gap: a pipeline detached because its connection closed stays detached until an offer
    // happens to be made, and if none is, the microphone is live and silent for the rest
    // of the call. Filling only empty slots means an active attachment is never stolen,
    // and `create_offer` still overwrites this if the real publishing connection turns out
    // to be a different one.
    adopt_idle_pipelines(&media_state, &state, &id);

    // Spawn a task to forward events from the I/O loop to the frontend. Instrumented
    // with the same span so events emitted while forwarding carry this connection's
    // correlation_id.
    let state_clone: WebRtcState = state.inner().clone();
    let id_clone = id.clone();
    let forward_span = span.clone();
    tokio::spawn(
        async move {
            forward_events(&state_clone, &app, &id_clone).await;
        }
        .instrument(forward_span),
    );

    Ok(PeerConnectionResult { id })
}

/// Point any unattached capture pipeline at this connection.
///
/// Deliberately only fills empty slots. A pipeline already feeding a live connection must
/// keep feeding it -- stealing it here would silence a working call whenever a second
/// connection is created, which happens routinely for data channels.
fn adopt_idle_pipelines(media_state: &MediaState, state: &WebRtcState, pc_id: &str) {
    let Ok(engine) = state.0.lock() else {
        return;
    };
    let Some(io_cmd_tx) = engine.get(pc_id).map(|managed| managed.io_cmd_tx.clone()) else {
        return;
    };
    drop(engine);

    // Every idle pipeline, not two named ones. A user who starts a share before the call
    // connects has three or four running, and adopting only "the camera" and "the
    // microphone" would leave the share capturing into a channel with no far end.
    let Ok(pipelines) = media_state.pipelines.lock() else {
        return;
    };
    for handle in pipelines.values() {
        if let Ok(mut slot) = handle.encode_tx.lock()
            && slot.is_none()
        {
            *slot = Some(io_cmd_tx.clone());
            tracing::info!(
                pc_id,
                key = %handle.key,
                "unattached capture pipeline adopted by a new peer connection"
            );
        }
    }
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
    /// The `MediaStreamTrack.id` of the track this transceiver sends, when there is one.
    ///
    /// livekit-client sends this same value as the `cid` in its `AddTrackRequest`, and the
    /// SFU pairs a published track with an m-line by matching the two. Without it str0m
    /// invents an msid track id the SFU has never seen, and the association falls back to
    /// the server's guess by media kind.
    pub track_id: Option<String>,
    /// Which of the user's tracks this transceiver sends: `camera`, `screen_share`, and so
    /// on, as the shim tagged it.
    ///
    /// The `MediaStreamTrack` id above is a browser UUID and says nothing about which track
    /// it is, so without this every sending video transceiver was labelled the camera and
    /// the screen share had to share its m-line -- which put the camera picture in the
    /// far end's screen-share tile and left the camera tile black.
    pub source: Option<String>,
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
        num_dc = data_channels.as_ref().map_or(0, Vec::len),
        num_tc = transceivers.as_ref().map_or(0, Vec::len),
        "Creating offer"
    );

    let (handle, io_cmd_tx) = {
        let engine = state.0.lock_str("create_offer")?;
        engine
            .get(&pc_id)
            .map(|managed| (managed.handle.clone(), managed.io_cmd_tx.clone()))
            .ok_or_else(|| pc_not_found("create_offer", &pc_id))?
    };

    // Connect every running capture pipeline to this PC's I/O channel.
    //
    // `video` used to gate whether the camera was attached at all. It no longer can: a
    // connection carrying a screen share is a video connection whether or not the camera
    // is on, and the pipelines that exist are the authority on what is being captured.
    // A failure to take this lock is reported, not skipped. Skipping attaches *nothing*:
    // the call proceeds, every pipeline keeps capturing into a channel with no peer
    // connection on the other end, and the user joins a call in which they are silent and
    // invisible while everything reports success. That is the loudest possible symptom
    // reached by the quietest possible path.
    let mut attached = 0_usize;
    match media_state.pipelines.lock() {
        Ok(pipelines) => {
            for handle in pipelines.values() {
                if handle.key.kind() == elementium_types::TrackKind::Video && !video {
                    continue;
                }
                match handle.encode_tx.lock() {
                    Ok(mut encode_guard) => {
                        tracing::info!(pc_id = %pc_id, key = %handle.key, "Connecting capture pipeline to peer connection");
                        *encode_guard = Some(io_cmd_tx.clone());
                        attached = attached.saturating_add(1);
                    }
                    Err(_) => {
                        tracing::error!(
                            pc_id = %pc_id,
                            key = %handle.key,
                            "a capture pipeline's connection slot is poisoned; this track \
                             will not reach the call"
                        );
                    }
                }
            }
        }
        Err(_) => {
            tracing::error!(
                pc_id = %pc_id,
                "the pipeline map is poisoned; no capture can be attached to this call"
            );
        }
    }
    tracing::info!(pc_id = %pc_id, attached, "capture pipelines attached to the peer connection");

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
        .map(|tc| {
            peer_connection::TransceiverInfo::from_js(
                &tc.kind,
                tc.direction.as_deref(),
                tc.track_id,
                tc.source.as_deref(),
            )
        })
        .collect();

    let mut pc = handle.lock_str("create_offer")?;
    peer_connection::create_offer(&mut pc, &dc_infos, &tc_infos).ipc_err("create_offer", &pc_id)
}

/// The "unknown peer connection" failure every lookup by `pc_id` in this module can hit,
/// logged once here so every call site gets the same message and the same log line rather
/// than re-deriving both.
///
/// `warn`, not `error`: a `pc_id` the frontend still holds after `close_peer_connection` (or
/// after a race between the two) is an ordinary, recoverable condition -- not evidence this
/// command itself malfunctioned -- so it must not compete in the log with a genuine failure
/// the way an `error`-level line would. See Principle II.
fn pc_not_found(command: &str, pc_id: &str) -> String {
    tracing::warn!(
        command,
        pc_id,
        "no peer connection is registered with this id; it may already be closed"
    );
    "Peer connection not found".to_owned()
}

/// Look up an active peer connection's handle by ID.
fn get_pc_handle(
    state: &WebRtcState,
    command: &str,
    pc_id: &str,
) -> Result<peer_connection::PeerConnectionHandle, String> {
    let engine = state.0.lock_str(command)?;
    engine
        .get(pc_id)
        .map(|managed| managed.handle.clone())
        .ok_or_else(|| pc_not_found(command, pc_id))
}

#[command]
pub async fn create_answer(
    state: State<'_, WebRtcState>,
    pc_id: String,
) -> Result<SessionDescription, String> {
    tracing::info!(pc_id = %pc_id, "Creating answer");
    let handle = get_pc_handle(&state, "create_answer", &pc_id)?;
    let mut pc = handle.lock_str("create_answer")?;
    peer_connection::create_answer(&mut pc).ipc_err("create_answer", &pc_id)
}

#[command]
pub async fn set_local_description(
    state: State<'_, WebRtcState>,
    pc_id: String,
    description: SessionDescription,
) -> Result<(), String> {
    // Accepted and not applied, which is correct here but only for one specific reason:
    // str0m has already applied this exact SDP. `create_offer` and `create_answer` set the
    // local description as they generate it -- there is no separate "apply" step in that
    // API, and re-applying would be a second, conflicting change to the same session.
    //
    // So this exists to satisfy the DOM contract the page follows, not to do work. The
    // distinction matters because the previous `let _ = (..)` looked like an oversight, and
    // the obvious "fix" -- plumbing it through to str0m -- would break negotiation.
    //
    // What is *not* fine is accepting a description that disagrees with what we generated.
    // The comment here used to claim that was "checked rather than assumed" while the body
    // did nothing of the kind -- a comment asserting a safety property nobody implemented,
    // which is worse than no comment, because the next person reads it and believes it.
    //
    // It is checked now, and reported rather than refused. livekit-client munges SDP
    // deliberately (the shim has a munging block of its own), so a difference is not
    // automatically a fault; but it does mean the bytes str0m is using are not the bytes
    // the page thinks it sent, and when that matters it matters a great deal. Reported as
    // lengths and a line number, never as SDP: these logs get attached to issues, and an
    // SDP carries the ICE credentials and DTLS fingerprint for the session.
    if description.sdp_type == SdpType::Offer
        && let Ok(handle) = get_pc_handle(&state, "set_local_description", &pc_id)
        && let Ok(pc) = handle.lock_str("set_local_description")
        && let Some(generated) = pc.last_offer_sdp.as_deref()
        && generated != description.sdp
    {
        let diverged_at = generated
            .lines()
            .zip(description.sdp.lines())
            .position(|(ours, theirs)| ours != theirs);
        tracing::warn!(
            pc_id = %pc_id,
            generated_len = generated.len(),
            returned_len = description.sdp.len(),
            first_differing_line = diverged_at.map_or_else(|| "(length only)".to_owned(), |n| n.to_string()),
            reason = "local_description_munged",
            "the page returned a local description that differs from the one we generated; \
             str0m is using ours"
        );
    }

    tracing::info!(
        pc_id = %pc_id,
        sdp_type = ?description.sdp_type,
        sdp_len = description.sdp.len(),
        "local description accepted; str0m applied it when it was generated"
    );
    Ok(())
}

#[command]
pub async fn set_remote_description(
    state: State<'_, WebRtcState>,
    pc_id: String,
    description: SessionDescription,
) -> Result<Option<SessionDescription>, String> {
    tracing::info!(pc_id = %pc_id, sdp_type = ?description.sdp_type, "Setting remote description");
    let handle = get_pc_handle(&state, "set_remote_description", &pc_id)?;
    let mut pc = handle.lock_str("set_remote_description")?;
    peer_connection::set_remote_description(&mut pc, &description)
        .ipc_err("set_remote_description", &pc_id)
}

#[command]
pub async fn add_ice_candidate(
    state: State<'_, WebRtcState>,
    pc_id: String,
    candidate: IceCandidate,
) -> Result<(), String> {
    tracing::info!(pc_id = %pc_id, candidate = %candidate.candidate, "Adding ICE candidate");
    let handle = get_pc_handle(&state, "add_ice_candidate", &pc_id)?;
    let mut pc = handle.lock_str("add_ice_candidate")?;
    peer_connection::add_ice_candidate(&mut pc, &candidate.candidate)
        .ipc_err("add_ice_candidate", &pc_id)
}

/// Write one message to a negotiated data channel.
///
/// This is the send half of `RTCDataChannel.send()`: before this command existed the
/// shim's `send()` was a no-op (`(_data) => {}`), so anything Element Call or
/// livekit-client wrote to a data channel silently vanished. `data` carries the payload as
/// raw bytes regardless of whether the page sent a string or an `ArrayBuffer`/typed array
/// -- `binary` is what tells the shim (and, on the wire, the far end's DCEP framing)
/// which one it originally was. `Blob` is not supported: the DOM allows it as a `send()`
/// argument, but nothing in this call chain (Tauri IPC, str0m's `Channel::write`) has a
/// notion of a `Blob`, and the shim converts it to bytes before this command is ever
/// invoked -- see `webrtc-shim.ts`.
///
/// Returns `Ok(false)` (not an error) when str0m's SCTP layer declines to accept the
/// buffer right now because it would not fit in the send window -- the caller can retry.
///
/// # Errors
///
/// A coded JSON envelope built from [`elementium_webrtc::error::DataChannelWriteError`]'s
/// `code`: `"data_channel_not_open"`/`"data_channel_no_stream"` for a channel the shim's
/// own `send()` should not have called this for yet (see that error type's doc), and
/// `"data_channel_write_failed"` for a real SCTP failure.
#[command]
pub async fn send_data_channel_message(
    state: State<'_, WebRtcState>,
    pc_id: String,
    label: String,
    binary: bool,
    data: Vec<u8>,
) -> Result<bool, String> {
    tracing::debug!(pc_id = %pc_id, %label, binary, len = data.len(), "Sending data channel message");
    let handle = get_pc_handle(&state, "send_data_channel_message", &pc_id)?;
    let mut pc = handle.lock_str("send_data_channel_message")?;
    peer_connection::write_data_channel(&mut pc, &label, binary, &data)
        .ipc_err_coded("write_data_channel", &pc_id)
}

/// One outbound (sent) track's transport stats, as returned to JS.
///
/// Field names match what `getStats()` needs to build a spec-shaped `RTCStatsReport`
/// entry; see [`peer_connection::OutboundTransportStats`] for what each is derived from
/// and why the optional ones are `None` rather than `0` when str0m has not reported them.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboundStatsResult {
    pub mid: String,
    pub kind: Option<String>,
    pub bytes_sent: u64,
    pub packets_sent: u64,
    pub nack_count: u64,
    pub pli_count: u64,
    pub fir_count: u64,
    pub round_trip_time_ms: Option<u64>,
    pub remote_packets_lost: Option<u64>,
    pub remote_jitter_rtp_units: Option<u32>,
}

/// One inbound (received) track's transport stats, as returned to JS. See
/// [`OutboundStatsResult`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InboundStatsResult {
    pub mid: String,
    pub kind: Option<String>,
    pub bytes_received: u64,
    pub packets_received: u64,
    pub nack_count: u64,
    pub pli_count: u64,
    pub fir_count: u64,
    pub loss_fraction: Option<f32>,
}

/// The selected ICE candidate pair's stats, as returned to JS.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidatePairStatsResult {
    pub round_trip_time_ms: Option<u64>,
    pub local_addr: Option<String>,
    pub remote_addr: Option<String>,
}

/// Everything `getStats()` needs for one peer connection, as last reported by str0m.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TransportStatsResult {
    pub outbound: Vec<OutboundStatsResult>,
    pub inbound: Vec<InboundStatsResult>,
    pub candidate_pair: Option<CandidatePairStatsResult>,
}

/// Real transport-level statistics for a peer connection (bytes/packets, RTCP loss and
/// RTT, the selected ICE candidate pair), for the shim's `RTCPeerConnection.getStats()`.
///
/// Reads a snapshot Rust already keeps updated from str0m's stats events -- see
/// [`peer_connection::transport_stats_snapshot`] -- rather than asking str0m for
/// anything, so this never blocks on the I/O loop and never reports a number that is not
/// really the most recent one seen.
#[command]
pub async fn get_transport_stats(
    state: State<'_, WebRtcState>,
    pc_id: String,
) -> Result<TransportStatsResult, String> {
    let handle = get_pc_handle(&state, "get_transport_stats", &pc_id)?;
    let pc = handle.lock_str("get_transport_stats")?;
    let snapshot = peer_connection::transport_stats_snapshot(&pc);
    drop(pc);

    Ok(TransportStatsResult {
        outbound: snapshot
            .outbound
            .into_iter()
            .map(|s| OutboundStatsResult {
                mid: s.mid,
                kind: s.kind.map(str::to_string),
                bytes_sent: s.bytes_sent,
                packets_sent: s.packets_sent,
                nack_count: s.nack_count,
                pli_count: s.pli_count,
                fir_count: s.fir_count,
                round_trip_time_ms: s.round_trip_time_ms,
                remote_packets_lost: s.remote_packets_lost,
                remote_jitter_rtp_units: s.remote_jitter_rtp_units,
            })
            .collect(),
        inbound: snapshot
            .inbound
            .into_iter()
            .map(|s| InboundStatsResult {
                mid: s.mid,
                kind: s.kind.map(str::to_string),
                bytes_received: s.bytes_received,
                packets_received: s.packets_received,
                nack_count: s.nack_count,
                pli_count: s.pli_count,
                fir_count: s.fir_count,
                loss_fraction: s.loss_fraction,
            })
            .collect(),
        candidate_pair: snapshot.candidate_pair.map(|c| CandidatePairStatsResult {
            round_trip_time_ms: c.round_trip_time_ms,
            local_addr: c.local_addr,
            remote_addr: c.remote_addr,
        }),
    })
}

/// Ask that the next offer carry fresh ICE credentials.
///
/// What `RTCPeerConnection.restartIce()` means: the request is recorded and the offer the
/// page makes next carries it. Before this the shim only logged, so a call could not
/// recover from a network change -- waking from suspend, moving from Wi-Fi to Ethernet --
/// and the only way back was a full reconnect.
///
/// # Errors
///
/// Returns an error if the peer connection is unknown, rather than succeeding quietly: a
/// caller told the restart was accepted will wait for a recovery that cannot come.
#[command]
pub async fn restart_ice(state: State<'_, WebRtcState>, pc_id: String) -> Result<(), String> {
    let handle = get_pc_handle(&state, "restart_ice", &pc_id)?;
    let mut pc = handle.lock_str("restart_ice")?;
    peer_connection::request_ice_restart(&mut pc);
    tracing::info!(pc_id = %pc_id, "ICE restart requested; the next offer will carry it");
    Ok(())
}

#[command]
pub async fn close_peer_connection(
    state: State<'_, WebRtcState>,
    pc_id: String,
) -> Result<(), String> {
    let span = {
        let mut engine = state.0.lock_str("close_peer_connection")?;
        let span = engine
            .get(&pc_id)
            .map_or_else(tracing::Span::current, |managed| managed.span.clone());
        engine.remove(&pc_id);
        span
    };
    let _enter = span.enter();
    tracing::info!(pc_id = %pc_id, "peer connection closed");
    Ok(())
}

/// Forward events from the I/O loop to the Tauri frontend via `webview.eval()`.
///
/// Uses `eval()` instead of the Tauri event system (`emit`/`listen`) because
/// Tauri's permission check blocks `listen()` from non-local URLs
/// (e.g. <http://localhost:5173> in dev mode). `eval()` bypasses this entirely.
/// Where a [`PcEvent`] can be routed to, besides the frontend.
struct Routing<'a> {
    app: &'a AppHandle,
    pc_id: &'a str,
    audio_tx: &'a tokio_mpsc::Sender<PcEvent>,
    video_tx: &'a tokio_mpsc::Sender<PcEvent>,
    /// Where an encoded video frame goes when the page is decoding for itself.
    encoded: &'a crate::encoded_streams::EncodedStreams,
}

/// Dispatch one peer-connection event.
///
/// Returns `Some` for events destined for the frontend, and `None` for those handled
/// entirely in the backend: media payloads go to their decode pipelines, and RTCP egress
/// stats feed the encoder's loss estimate. None of those are meaningful to JS.
fn route_pc_event(
    event: PcEvent,
    routing: &Routing<'_>,
    stats: &mut ForwardStats,
) -> Option<WebRtcEvent> {
    let pc_id = routing.pc_id;
    match event {
        PcEvent::IceConnectionStateChange(s) => Some(WebRtcEvent::IceConnectionStateChange {
            pc_id: pc_id.to_string(),
            state: format!("{s:?}").to_lowercase(),
        }),
        PcEvent::ConnectionStateChange(s) => Some(WebRtcEvent::ConnectionStateChange {
            pc_id: pc_id.to_string(),
            state: format!("{s:?}").to_lowercase(),
        }),
        PcEvent::IceCandidate(candidate) => Some(WebRtcEvent::IceCandidate {
            pc_id: pc_id.to_string(),
            candidate,
        }),
        PcEvent::IceGatheringComplete => Some(WebRtcEvent::IceGatheringComplete {
            pc_id: pc_id.to_string(),
        }),
        PcEvent::Connected => Some(WebRtcEvent::Connected {
            pc_id: pc_id.to_string(),
        }),
        PcEvent::RemoteTrackAdded {
            mid,
            kind,
            stream_id,
        } => Some(WebRtcEvent::RemoteTrackAdded {
            pc_id: pc_id.to_string(),
            mid,
            kind,
            stream_id,
        }),
        PcEvent::AudioData {
            mid,
            data,
            contiguous,
        } => {
            // Counted, not logged: at fifty packets a second per track, a line per drop
            // costs more than the drop it reports (see the same lesson in `engine.rs`'s
            // `dropped_events`). `ForwardStats::report` carries the count instead.
            let ev = PcEvent::AudioData {
                mid,
                data,
                contiguous,
            };
            if routing.audio_tx.try_send(ev).is_err() {
                stats.dropped_audio = stats.dropped_audio.saturating_add(1);
            }
            None
        }
        PcEvent::VideoData { mid, data, codec } => {
            // When the page has opened a stream for this track it is decoding for itself, and
            // the encoded frame goes there instead of to our decoder: 20-60kB rather than the
            // 3.7MB of RGBA that decoding it here would produce, and the webview's own
            // hardware path instead of our software VP8.
            //
            // The frames are still decrypted -- this is downstream of the E2EE boundary and
            // the type says so -- so nothing about the encryption model changes.
            let track_key = format!("{}-{}", routing.pc_id, mid);
            if routing.encoded.has_reader(&track_key) {
                routing.encoded.push_encoded(&track_key, data);
                return None;
            }
            // Counted per track key, because a stream that decodes nothing and a stream that
            // was never offered anything look identical from the page. If a track the page
            // has open shows up here, the two sides disagree about its key -- which is a
            // routing fault, not a decoder one.
            stats.note_unrouted_video(&track_key);
            // Was `let _ =`: video dropped between here and the decoder was invisible, so a
            // decoder that could not keep up looked exactly like a peer that sent nothing.
            if routing
                .video_tx
                .try_send(PcEvent::VideoData { mid, data, codec })
                .is_err()
            {
                stats.dropped_video = stats.dropped_video.saturating_add(1);
            }
            None
        }
        PcEvent::EgressStats {
            mid,
            loss,
            rtt_ms,
            packets,
            nacks,
        } => {
            record_outbound_loss(routing.app, pc_id, &mid, loss, rtt_ms, packets, nacks);
            None
        }
        PcEvent::KeyframeRequested { mid } => {
            request_camera_keyframe(routing.app, pc_id, &mid);
            None
        }
    }
}

/// Tell the camera encoder that a receiver cannot decode us.
///
/// Until this existed the request was logged and dropped, and the encoder emitted a
/// keyframe only on its own three-second timer -- so a receiver that lost one spent up to
/// three seconds displaying a broken picture and asking again, repeatedly. A real log shows
/// exactly that: PLIs at 06:16:14, :14.6, :15.2, :18.6, :21.7 with nothing in between.
fn request_camera_keyframe(app: &AppHandle, pc_id: &str, mid: &str) {
    let media_state = app.state::<MediaState>();
    let Ok(pipelines) = media_state.pipelines.lock() else {
        return;
    };
    // Asked of every video pipeline rather than of the one this mid belongs to.
    //
    // Resolving mid to track would mean reaching into the peer connection's published-mid
    // map from here, and the cost of not doing so is one extra keyframe on the other
    // track. The cost of getting it wrong in the other direction is a receiver that asks
    // for a keyframe forever and never gets one, which is an indefinite black picture.
    let mut asked = 0_usize;
    for handle in pipelines.values() {
        if let Some((keyframe_requested, _)) = handle.video() {
            keyframe_requested.store(true, Ordering::Relaxed);
            asked = asked.saturating_add(1);
        }
    }
    if asked == 0 {
        tracing::debug!(pc_id, mid, "keyframe requested with no video running");
        return;
    }
    tracing::debug!(
        pc_id,
        mid,
        asked,
        "keyframe request passed to video encoders"
    );
}

/// Feed loss measured by the peers back into the Opus encoder's FEC sizing.
///
/// Closes a loop that previously did not exist: the encoder protected against a hardcoded
/// guess while the peers were telling us, once per second in their RTCP receiver reports,
/// what the real loss was.
///
/// `loss` of `None` means no report arrived during the interval. That is deliberately not
/// treated as zero: folding in 0.0 would decay the estimate toward "clean link" purely
/// because reports stopped arriving, which is the opposite of the truth if the link just
/// got bad enough to lose the reports too.
#[allow(clippy::too_many_arguments)]
fn record_outbound_loss(
    app: &AppHandle,
    pc_id: &str,
    mid: &str,
    loss: Option<f32>,
    rtt_ms: Option<u64>,
    packets: u64,
    nacks: u64,
) {
    let Some(fraction) = loss else {
        return;
    };
    let media_state = app.state::<MediaState>();
    let Ok(pipelines) = media_state.pipelines.lock() else {
        return;
    };
    // Every audio pipeline: loss is a property of the link, not of one track, so the
    // microphone and a shared application's audio are both encoding over the same
    // conditions and both want their FEC sized for them.
    for handle in pipelines.values() {
        let Some(loss_estimate) = handle.loss_estimate() else {
            continue;
        };
        loss_estimate.observe(fraction);
        tracing::debug!(
            pc_id,
            mid,
            key = %handle.key,
            reported_loss = fraction,
            smoothed_perc = loss_estimate.percent(),
            rtt_ms = ?rtt_ms,
            packets,
            nacks,
            "Outbound loss measured from RTCP receiver report"
        );
    }
}

/// How many media events to take in one batch before yielding to the other select arms.
///
/// The channel's own capacity, so a full channel empties in a single pass. The per-pass
/// costs this loop pays -- polling the other arms, and previously three mutex acquisitions
/// and a hash lookup -- were once paid *per event*, which is what put a ceiling on the
/// drain rate well below the rate a remote participant's video arrives at.
const MEDIA_DRAIN_BATCH: usize = 256;

/// How often to check the connection for queued data-channel events and for liveness.
///
/// Data-channel events are not on a channel of their own: they queue on the connection and
/// must be picked up under its lock -- the same lock the I/O loop holds while decrypting
/// and depacketising inbound media. Taking it per media event made the forwarder and the
/// producer contend for it thousands of times a second. At this cadence they contend fifty
/// times a second instead, and the added latency is bounded by this interval.
const DATA_CHANNEL_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// How often to report forwarder throughput. See [`ForwardStats`].
const FORWARD_REPORT_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

/// What the forwarder managed to move, so a stall is visible as a number rather than
/// inferred from the absence of pictures.
///
/// Every field here answers a question that was previously unanswerable from a log: how
/// much got through, whether it arrived in drainable batches or one at a time, and where it
/// was lost if it was lost. The video drop count in particular replaces a bare `let _ =`.
#[derive(Default)]
struct ForwardStats {
    media_events: u64,
    control_events: u64,
    batches: u64,
    max_batch: usize,
    dropped_audio: u64,
    dropped_video: u64,
    /// Video frames that found no encoded-stream reader, by track key.
    ///
    /// Expected to be busy while the native decode path is in use -- that is the normal
    /// case. It earns its place when the page *has* opened a stream for a key that appears
    /// here anyway: the two sides then disagree about the key, and nothing else in either
    /// log says so.
    unrouted_video: std::collections::BTreeMap<String, u64>,
}

impl ForwardStats {
    /// Note a video frame that no page-side stream claimed.
    fn note_unrouted_video(&mut self, track_key: &str) {
        // Bounded by the number of remote tracks on one connection, which is small; a key
        // that is already present is the overwhelmingly common case.
        if let Some(count) = self.unrouted_video.get_mut(track_key) {
            *count = count.saturating_add(1);
        } else if self.unrouted_video.len() < 32 {
            self.unrouted_video.insert(track_key.to_owned(), 1);
        }
    }

    /// Report and reset, so each line describes the interval it covers rather than all of
    /// time -- a cumulative counter cannot show that a stall started.
    fn report(&mut self, pc_id: &str, elapsed: std::time::Duration) {
        // Silence is the healthy state for an idle connection and there is nothing to say
        // about it. Reporting only when something moved (or was lost) keeps the interval
        // line meaningful instead of one every ten seconds per connection forever.
        if self.media_events == 0 && self.control_events == 0 && self.dropped_video == 0 {
            return;
        }
        // Integer throughout: a rate is easier to read than a count and an interval, and
        // the workspace forbids `as` conversions, which is the only cheap way to a float
        // here.
        let millis = u64::try_from(elapsed.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        let per_sec = self
            .media_events
            .saturating_mul(1000)
            .checked_div(millis)
            .unwrap_or(0);
        tracing::info!(
            pc_id,
            media_events = self.media_events,
            media_per_sec = per_sec,
            control_events = self.control_events,
            batches = self.batches,
            max_batch = self.max_batch,
            dropped_audio = self.dropped_audio,
            dropped_video = self.dropped_video,
            unrouted_video = ?self.unrouted_video,
            "Inbound event forwarding"
        );
        *self = Self::default();
    }
}

async fn forward_events(state: &WebRtcState, app: &AppHandle, pc_id: &str) {
    // Create relay channels for audio and video playback pipelines
    let (audio_tx, audio_rx) = tokio_mpsc::channel::<PcEvent>(256);
    let (video_tx, video_rx) = tokio_mpsc::channel::<PcEvent>(256);

    // Take the event receivers, the video frame buffer and the shared audio output stream
    // from the engine, once. The audio player is shared across every PeerConnection this
    // engine manages (not one per connection) -- see `WebRtcEngine::shared_audio_player`.
    //
    // The receivers are *moved* here rather than borrowed per event: owning them is what
    // lets this loop wait on `recv()` instead of polling `try_recv()` behind a mutex, and
    // waiting is what removes both the 10ms idle sleep and the spin that starved the
    // runtime whenever media was flowing.
    let (video_frames, shared_player, receivers, handle) = {
        let Ok(engine) = state.0.lock() else {
            return;
        };
        let Some(managed) = engine.get(pc_id) else {
            return;
        };
        let Some(receivers) = managed.take_receivers() else {
            tracing::error!(
                pc_id,
                "event forwarding is already running for this connection"
            );
            return;
        };
        (
            engine.video_frames.clone(),
            engine.shared_audio_player(),
            receivers,
            managed.handle.clone(),
        )
    };
    let elementium_webrtc::engine::PcReceivers {
        mut event_rx,
        mut control_rx,
    } = receivers;

    // Start audio playback pipeline (receives Opus → decodes → cpal output)
    match shared_player {
        Some(player) => {
            // Infallible: decode failures are handled and logged inside the pipeline rather
            // than propagated, so there is no error here to handle. It used to return a
            // `Result` that was always `Ok`, which taught this call site to write a branch
            // that could never run.
            start_audio_playback(audio_rx, pc_id.to_string(), player);
        }
        None => {
            tracing::error!(
                pc_id = pc_id,
                "No shared audio player available; inbound audio for this connection will not play"
            );
        }
    }

    // Start video playback pipeline (receives VP8 → decodes → frame buffer)
    let mut video_pipeline = VideoPipeline::new();
    // Infallible, for the same reason as the audio pipeline above.
    video_pipeline.start_playback(video_rx, video_frames, pc_id.to_string());

    let encoded = app
        .try_state::<crate::encoded_streams::EncodedStreams>()
        .map_or_else(crate::encoded_streams::EncodedStreams::new, |s| (*s).clone());
    let routing = Routing {
        app,
        pc_id,
        audio_tx: &audio_tx,
        video_tx: &video_tx,
        encoded: &encoded,
    };
    let mut metrics = ForwardStats::default();
    let mut last_report = tokio::time::Instant::now();

    // Ticks even when no media is flowing, because it is also this loop's liveness check:
    // the media and control channels close when the I/O loop exits, but a connection
    // removed from the engine while its I/O loop lingers would otherwise leave this task
    // forwarding to nobody.
    let mut housekeeping = tokio::time::interval(DATA_CHANNEL_POLL);
    housekeeping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // A closed channel yields `None` immediately and forever. Without these the matching
    // select arm would be re-polled every iteration and complete instantly, turning
    // shutdown into a hot spin -- the exact failure this rewrite exists to remove.
    let mut media_open = true;
    let mut control_open = true;

    loop {
        tokio::select! {
            // Control first, deliberately. These are the transitions media is *about* --
            // a track added, a connection state changed -- and a burst of media must never
            // delay one. `biased` makes that a guarantee rather than a coin flip.
            biased;

            control = control_rx.recv(), if control_open => {
                match control {
                    Some(event) => drain_control(event, &mut control_rx, &routing, &mut metrics),
                    None => control_open = false,
                }
            }

            media = event_rx.recv(), if media_open => {
                match media {
                    Some(event) => drain_media(event, &mut event_rx, &routing, &mut metrics),
                    None => media_open = false,
                }
            }

            _ = housekeeping.tick() => {
                if !drain_data_channels(state, app, pc_id, &handle) {
                    return;
                }
                let elapsed = last_report.elapsed();
                if elapsed >= FORWARD_REPORT_EVERY {
                    metrics.report(pc_id, elapsed);
                    last_report = tokio::time::Instant::now();
                }
                // Both event sources are gone and nothing more can arrive. Returning here
                // rather than lingering is what stops a reconnect from leaving a forwarder
                // per closed connection behind it.
                if !media_open && !control_open {
                    metrics.report(pc_id, last_report.elapsed());
                    tracing::info!(pc_id, "event forwarding stopped: both event channels closed");
                    return;
                }
            }
        }
    }
}

/// Forward one control event and everything queued behind it, in arrival order.
///
/// Unlike media there is no backpressure concern in taking these as fast as they arrive:
/// the channel is unbounded precisely because none of them may be dropped.
fn drain_control(
    first: PcEvent,
    control_rx: &mut tokio_mpsc::UnboundedReceiver<PcEvent>,
    routing: &Routing<'_>,
    metrics: &mut ForwardStats,
) {
    metrics.control_events = metrics.control_events.saturating_add(1);
    route_and_emit(first, routing, metrics);
    while let Ok(event) = control_rx.try_recv() {
        metrics.control_events = metrics.control_events.saturating_add(1);
        route_and_emit(event, routing, metrics);
    }
}

/// Forward one media event and up to [`MEDIA_DRAIN_BATCH`] more queued behind it.
///
/// Bounded rather than unbounded so a saturated channel cannot hold this task in a loop
/// indefinitely while control events and data channels wait: the cap is the channel's own
/// capacity, so a full channel still empties in one pass, but a producer faster than this
/// consumer cannot starve the other select arms.
fn drain_media(
    first: PcEvent,
    event_rx: &mut tokio_mpsc::Receiver<PcEvent>,
    routing: &Routing<'_>,
    metrics: &mut ForwardStats,
) {
    route_and_emit(first, routing, metrics);
    let mut batch = 1usize;
    while batch < MEDIA_DRAIN_BATCH {
        let Ok(event) = event_rx.try_recv() else {
            break;
        };
        route_and_emit(event, routing, metrics);
        batch = batch.saturating_add(1);
    }
    metrics.media_events = metrics
        .media_events
        .saturating_add(u64::try_from(batch).unwrap_or(u64::MAX));
    metrics.batches = metrics.batches.saturating_add(1);
    metrics.max_batch = metrics.max_batch.max(batch);
}

/// Pick up whatever data-channel events the connection has queued, and report whether the
/// connection is still one this task should be forwarding for.
///
/// Data-channel events are not on either event channel: they never pass through the E2EE
/// boundary `PcEvent` exists for (see `DataChannelEvent`), so they queue on the connection
/// itself and have to be collected under its lock.
///
/// Returns `false` when the connection has left the engine or a lock is poisoned, telling
/// the caller to stop forwarding for this `pc_id` entirely.
fn drain_data_channels(
    state: &WebRtcState,
    app: &AppHandle,
    pc_id: &str,
    handle: &elementium_webrtc::peer_connection::PeerConnectionHandle,
) -> bool {
    {
        let Ok(engine) = state.0.lock() else {
            return false;
        };
        if engine.get(pc_id).is_none() {
            return false;
        }
    }

    // Taken *after* the engine lock is released, never nested inside it: the I/O loop holds
    // this connection's lock for the whole of each poll, and holding the engine lock while
    // waiting for it would stall every other connection and every command that needs the
    // engine.
    let Ok(mut pc) = handle.lock() else {
        return false;
    };
    let events = peer_connection::drain_data_channel_events(&mut pc);
    drop(pc);

    for event in events {
        emit_to_frontend(app, pc_id, &data_channel_event_to_webrtc(pc_id, event));
    }
    true
}

/// Route one [`PcEvent`] and, if it maps to a frontend event, emit it.
///
/// Split out of `forward_events` because the media and control paths both need exactly
/// this sequence and would otherwise duplicate it.
fn route_and_emit(event: PcEvent, routing: &Routing<'_>, stats: &mut ForwardStats) {
    if let Some(tauri_event) = route_pc_event(event, routing, stats) {
        emit_to_frontend(routing.app, routing.pc_id, &tauri_event);
    }
}

/// Push one event to JS via `eval()` -- calls the global handler registered by the WebRTC
/// shim on `window.top`.
///
/// Uses `eval()` instead of the Tauri event system (`emit`/`listen`) because Tauri's
/// permission check blocks `listen()` from non-local URLs (e.g. `http://localhost:5173` in
/// dev mode). `eval()` bypasses this entirely.
fn emit_to_frontend(app: &AppHandle, pc_id: &str, event: &WebRtcEvent) {
    let Some(webview) = app.get_webview_window("main") else {
        tracing::error!("No 'main' webview found for eval");
        return;
    };
    let Ok(json) = serde_json::to_string(event) else {
        tracing::error!(pc_id, "Failed to serialize event for eval");
        return;
    };
    let js =
        format!("if(window.__elementium_webrtc_event)window.__elementium_webrtc_event({json})");
    match webview.eval(&js) {
        Ok(()) => tracing::debug!(pc_id, "eval sent to JS"),
        Err(e) => tracing::error!(pc_id, err = %e, "eval failed"),
    }
}

fn generate_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("pc-{t:x}")
}
