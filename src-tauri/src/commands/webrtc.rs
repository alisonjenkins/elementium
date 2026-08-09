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

use super::LockExt;
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
fn data_channel_event_to_webrtc(pc_id: &str, event: peer_connection::DataChannelEvent) -> WebRtcEvent {
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
        let mut engine = state.0.lock_str()?;
        engine.create_connection(id.clone(), ice_servers.as_deref())?;
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

    let (handle, io_cmd_tx) = state
        .0
        .lock()
        .map_err(|e| e.to_string())?
        .get(&pc_id)
        .ok_or("Peer connection not found")
        .map(|managed| (managed.handle.clone(), managed.io_cmd_tx.clone()))?;

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

    let mut pc = handle.lock_str()?;
    Ok(peer_connection::create_offer(
        &mut pc, &dc_infos, &tc_infos,
    )?)
}

/// Look up an active peer connection's handle by ID.
fn get_pc_handle(
    state: &WebRtcState,
    pc_id: &str,
) -> Result<peer_connection::PeerConnectionHandle, String> {
    let engine = state.0.lock_str()?;
    Ok(engine
        .get(pc_id)
        .ok_or("Peer connection not found")?
        .handle
        .clone())
}

#[command]
pub async fn create_answer(
    state: State<'_, WebRtcState>,
    pc_id: String,
) -> Result<SessionDescription, String> {
    tracing::info!(pc_id = %pc_id, "Creating answer");
    let handle = get_pc_handle(&state, &pc_id)?;
    let mut pc = handle.lock_str()?;
    Ok(peer_connection::create_answer(&mut pc)?)
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
        && let Ok(handle) = get_pc_handle(&state, &pc_id)
        && let Ok(pc) = handle.lock_str()
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
    let handle = get_pc_handle(&state, &pc_id)?;
    let mut pc = handle.lock_str()?;
    Ok(peer_connection::set_remote_description(
        &mut pc,
        &description,
    )?)
}

#[command]
pub async fn add_ice_candidate(
    state: State<'_, WebRtcState>,
    pc_id: String,
    candidate: IceCandidate,
) -> Result<(), String> {
    tracing::info!(pc_id = %pc_id, candidate = %candidate.candidate, "Adding ICE candidate");
    let handle = get_pc_handle(&state, &pc_id)?;
    let mut pc = handle.lock_str()?;
    Ok(peer_connection::add_ice_candidate(
        &mut pc,
        &candidate.candidate,
    )?)
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
#[command]
pub async fn send_data_channel_message(
    state: State<'_, WebRtcState>,
    pc_id: String,
    label: String,
    binary: bool,
    data: Vec<u8>,
) -> Result<bool, String> {
    tracing::debug!(pc_id = %pc_id, %label, binary, len = data.len(), "Sending data channel message");
    let handle = get_pc_handle(&state, &pc_id)?;
    let mut pc = handle.lock_str()?;
    Ok(peer_connection::write_data_channel(
        &mut pc, &label, binary, &data,
    )?)
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
    let handle = get_pc_handle(&state, &pc_id)?;
    let pc = handle.lock_str()?;
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
    let handle = get_pc_handle(&state, &pc_id)?;
    let mut pc = handle.lock_str()?;
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
        let mut engine = state.0.lock_str()?;
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
}

/// Dispatch one peer-connection event.
///
/// Returns `Some` for events destined for the frontend, and `None` for those handled
/// entirely in the backend: media payloads go to their decode pipelines, and RTCP egress
/// stats feed the encoder's loss estimate. None of those are meaningful to JS.
fn route_pc_event(
    event: PcEvent,
    routing: &Routing<'_>,
    dropped_audio_events: &mut u64,
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
        PcEvent::RemoteTrackAdded { mid, kind } => Some(WebRtcEvent::RemoteTrackAdded {
            pc_id: pc_id.to_string(),
            mid,
            kind,
        }),
        PcEvent::AudioData {
            mid,
            data,
            contiguous,
        } => {
            let ev = PcEvent::AudioData {
                mid: mid.clone(),
                data,
                contiguous,
            };
            if routing.audio_tx.try_send(ev).is_err() {
                *dropped_audio_events = dropped_audio_events.saturating_add(1);
                tracing::warn!(
                    pc_id,
                    mid,
                    dropped_audio_events = *dropped_audio_events,
                    "AudioData dropped: audio_tx full"
                );
            }
            None
        }
        PcEvent::VideoData { mid, data, codec } => {
            let _ = routing
                .video_tx
                .try_send(PcEvent::VideoData { mid, data, codec });
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
    tracing::debug!(pc_id, mid, asked, "keyframe request passed to video encoders");
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

async fn forward_events(state: &WebRtcState, app: &AppHandle, pc_id: &str) {
    // Create relay channels for audio and video playback pipelines
    let (audio_tx, audio_rx) = tokio_mpsc::channel::<PcEvent>(256);
    let (video_tx, video_rx) = tokio_mpsc::channel::<PcEvent>(256);

    // Get video frame buffer and the shared audio output stream from the engine. The
    // audio player is shared across every PeerConnection this engine manages (not one
    // per connection) -- see `WebRtcEngine::shared_audio_player` for why.
    let (video_frames, shared_player) = {
        let Ok(engine) = state.0.lock() else {
            return;
        };
        (engine.video_frames.clone(), engine.shared_audio_player())
    };

    // Start audio playback pipeline (receives Opus → decodes → cpal output)
    let audio_rx = Arc::new(Mutex::new(audio_rx));
    match shared_player {
        Some(player) => {
            if let Err(e) = start_audio_playback(audio_rx, pc_id.to_string(), player) {
                tracing::error!(pc_id = pc_id, "Failed to start audio playback: {e}");
            }
        }
        None => {
            tracing::error!(
                pc_id = pc_id,
                "No shared audio player available; inbound audio for this connection will not play"
            );
        }
    }

    // Start video playback pipeline (receives VP8 → decodes → frame buffer)
    let video_rx = Arc::new(Mutex::new(video_rx));
    let mut video_pipeline = VideoPipeline::new();
    if let Err(e) = video_pipeline.start_playback(video_rx, video_frames, pc_id.to_string()) {
        tracing::error!(pc_id = pc_id, "Failed to start video playback: {e}");
    }

    // Count of `PcEvent::AudioData` dropped because `audio_tx` (this loop -> the audio
    // playback thread, capacity 256) was full -- another gap invisible to str0m's own
    // `MediaData::contiguous` flag, since the drop happens after str0m already emitted a
    // complete, contiguous event.
    let mut dropped_audio_events: u64 = 0;

    loop {
        let Some((event, control_events, dc_events)) = drain_pc_queues(state, pc_id) else {
            return;
        };

        let routing = Routing {
            app,
            pc_id,
            audio_tx: &audio_tx,
            video_tx: &video_tx,
        };
        let mut sent_anything = false;

        if let Some(pc_event) = event {
            sent_anything = true;
            route_and_emit(pc_event, &routing, &mut dropped_audio_events);
        }

        // Every pending control event, not just one -- unlike the bounded media event
        // above, there is no backpressure concern in taking them as fast as they arrive,
        // and draining the whole queue here (in the order `try_recv` returns them, which
        // is arrival order) is what keeps them in the order they happened.
        for control_event in control_events {
            sent_anything = true;
            route_and_emit(control_event, &routing, &mut dropped_audio_events);
        }

        for dc_event in dc_events {
            sent_anything = true;
            emit_to_frontend(app, pc_id, &data_channel_event_to_webrtc(pc_id, dc_event));
        }

        if !sent_anything {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

/// Route one [`PcEvent`] and, if it maps to a frontend event, emit it.
///
/// Split out of `forward_events` because the media and control paths both need exactly
/// this sequence and would otherwise duplicate it.
fn route_and_emit(event: PcEvent, routing: &Routing<'_>, dropped_audio_events: &mut u64) {
    if let Some(tauri_event) = route_pc_event(event, routing, dropped_audio_events) {
        emit_to_frontend(routing.app, routing.pc_id, &tauri_event);
    }
}

/// One pass of pulling everything currently queued for `pc_id`: at most one media event
/// (`event_rx` is bounded and droppable, so only the newest is worth taking per pass),
/// every pending control event (`control_rx` is unbounded and undroppable), and every
/// pending data-channel event. Data-channel events are not on `event_rx`: they never pass
/// through the E2EE boundary `PcEvent` exists for (see `DataChannelEvent`), so they are
/// queued straight on the connection and drained here instead, under the same lock that
/// already has to be taken to reach the other two.
///
/// Returns `None` when the connection is gone or a lock is poisoned, telling the caller
/// to stop forwarding for this `pc_id` entirely.
#[allow(clippy::type_complexity)]
fn drain_pc_queues(
    state: &WebRtcState,
    pc_id: &str,
) -> Option<(
    Option<PcEvent>,
    Vec<PcEvent>,
    Vec<peer_connection::DataChannelEvent>,
)> {
    let engine = state.0.lock().ok()?;
    let managed = engine.get(pc_id)?;

    let pc_event = managed.event_rx.lock().ok()?.try_recv().ok();

    let control_events = {
        let mut rx = managed.control_rx.lock().ok()?;
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        events
    };

    let mut pc = managed.handle.lock().ok()?;
    let dc_events = peer_connection::drain_data_channel_events(&mut pc);
    drop(pc);
    drop(engine);

    Some((pc_event, control_events, dc_events))
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
