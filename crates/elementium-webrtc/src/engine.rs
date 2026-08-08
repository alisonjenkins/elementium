use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use elementium_e2ee::{E2eeContext, MediaKind as E2eeMediaKind};
use elementium_media::audio_playback::AudioSink;
use elementium_types::{PlaintextMedia, VideoFrame};

use crate::e2ee_io::{EncryptionPolicy, encrypt_or_drop, maybe_decrypt_event};
use crate::peer_connection::{
    self, PcEvent, PeerConnectionHandle, discover_and_add_srflx, lock_pc,
};

/// ICE server configuration (STUN/TURN) passed from the signaling layer.
#[derive(Debug, Clone)]
pub struct IceServerConfig {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

/// Command sent to the I/O loop task.
pub enum IoCommand {
    /// Write an encoded Opus frame to the peer connection. Carries [`PlaintextMedia`]
    /// so it cannot reach the socket without passing through encryption first.
    WriteAudio(PlaintextMedia),
    /// Write an encoded VP8 frame to the peer connection. See [`IoCommand::WriteAudio`].
    WriteVideo(PlaintextMedia),
    /// Shut down the I/O loop.
    Shutdown,
}

/// A shared buffer of the latest decoded video frame per track.
/// The protocol handler reads from this to serve frames to the webview.
pub type VideoFrameBuffer = Arc<Mutex<HashMap<String, VideoFrame>>>;

/// Info about a managed peer connection.
pub struct ManagedPc {
    pub handle: PeerConnectionHandle,
    pub socket: Arc<UdpSocket>,
    pub io_cmd_tx: mpsc::Sender<IoCommand>,
    pub event_rx: Arc<Mutex<mpsc::Receiver<PcEvent>>>,
    /// The `peer_connection` tracing span this connection was created under, carrying
    /// its `correlation_id`. Retained so later operations on this connection (e.g.
    /// closing it) can re-enter the same span.
    pub span: tracing::Span,
}

/// The WebRTC engine manages all active peer connections.
pub struct WebRtcEngine {
    connections: HashMap<String, ManagedPc>,
    /// Shared video frame buffer for all connections.
    pub video_frames: VideoFrameBuffer,
    /// Shared E2EE policy for frame encryption/decryption.
    /// Uses `Arc<Mutex<>>` so it can be shared with Tauri's `E2eeState` and
    /// populated after I/O loops are already running.
    pub e2ee: Arc<Mutex<EncryptionPolicy>>,
}

impl Default for WebRtcEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl WebRtcEngine {
    #[must_use]
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
            video_frames: Arc::new(Mutex::new(HashMap::new())),
            e2ee: Arc::new(Mutex::new(EncryptionPolicy::default())),
        }
    }

    /// Get the process-wide shared audio output stream handle (starting it on first call
    /// anywhere in the process). See [`elementium_media::audio_playback::shared_sink`] for
    /// why this is a process-wide singleton, not per-engine: a single call can involve
    /// more than one native `PeerConnection` (confirmed via a real session log), and each
    /// connection used to open its own output stream, which is a real, confirmed source
    /// of audio glitching independent of decode correctness.
    #[must_use]
    pub fn shared_audio_player(&self) -> Option<AudioSink> {
        elementium_media::audio_playback::shared_sink()
    }

    /// Create a new peer connection. Binds a UDP socket and starts the I/O loop.
    ///
    /// If `ice_servers` are provided, performs STUN discovery to find the
    /// server-reflexive (public) address and adds it as an srflx candidate.
    ///
    /// # Errors
    ///
    /// Returns an error if the UDP socket cannot be bound or its local
    /// address cannot be determined.
    pub fn create_connection(
        &mut self,
        id: String,
        ice_servers: Option<&[IceServerConfig]>,
    ) -> Result<(), crate::error::WebRtcError> {
        // Captured here (rather than at `spawn_blocking` time) so it reflects whatever
        // span the caller was in when it asked for this connection to be created — in
        // practice the `peer_connection` span entered by the Tauri command, carrying
        // this connection's `correlation_id`.
        let span = tracing::Span::current();

        let mut pc_inner = peer_connection::create_peer_connection(id.clone());

        // Bind a UDP socket for this connection
        let socket =
            UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("Failed to bind socket: {e}"))?;
        let local_addr = socket.local_addr().map_err(|e| e.to_string())?;

        // Add the socket address as a local ICE candidate (host)
        peer_connection::add_local_candidate(&mut pc_inner, local_addr);

        // Perform STUN discovery using provided ICE servers
        if let Some(servers) = ice_servers {
            discover_and_add_srflx(&socket, &mut pc_inner, local_addr, servers);
        }

        let handle: PeerConnectionHandle = Arc::new(Mutex::new(pc_inner));
        let socket = Arc::new(socket);

        // Channels for the I/O loop
        let (io_cmd_tx, io_cmd_rx) = mpsc::channel::<IoCommand>(256);
        let (event_tx, event_rx) = mpsc::channel::<PcEvent>(256);

        // Spawn the I/O loop as a blocking task (it does synchronous UDP I/O)
        let loop_handle = handle.clone();
        let loop_socket = socket.clone();
        let loop_e2ee = self.e2ee.clone(); // clones the Arc, shares the policy
        let loop_span = span.clone();
        tokio::task::spawn_blocking(move || {
            let _enter = loop_span.enter();
            io_loop(&loop_handle, &loop_socket, io_cmd_rx, &event_tx, &loop_e2ee);
        });

        self.connections.insert(
            id,
            ManagedPc {
                handle,
                socket,
                io_cmd_tx,
                event_rx: Arc::new(Mutex::new(event_rx)),
                span,
            },
        );

        Ok(())
    }

    /// Get a reference to a managed peer connection.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&ManagedPc> {
        self.connections.get(id)
    }

    /// Remove and shut down a peer connection.
    pub fn remove(&mut self, id: &str) -> Option<ManagedPc> {
        if let Some(managed) = self.connections.remove(id) {
            let span = managed.span.clone();
            let _enter = span.enter();
            tracing::info!(pc_id = %id, "peer connection removed from engine");
            let _ = managed.io_cmd_tx.try_send(IoCommand::Shutdown);
            // Clean up video frames for this connection
            if let Ok(mut frames) = self.video_frames.lock() {
                frames.retain(|k, _| !k.starts_with(id));
            }
            Some(managed)
        } else {
            None
        }
    }

    /// Get the IDs of all active connections.
    #[must_use]
    pub fn connection_ids(&self) -> Vec<String> {
        self.connections.keys().cloned().collect()
    }
}

/// How evenly audio frames actually reach str0m, which is when they reach the wire.
///
/// str0m runs with `PacerImpl::null()` (we do not enable BWE), so it transmits each packet
/// the moment it is handed over. The capture thread already measures its *own* output
/// cadence, and after fixing the input buffer that is a clean frame every 20ms -- but that
/// is not the wire cadence. Between the two sits this I/O loop, which blocks in
/// `recv_and_feed` for up to 20ms and then drains every queued command at once. If it does,
/// smoothly-produced frames are handed to str0m in clumps and transmitted in clumps, and
/// the far end's jitter buffer conceals the gaps -- robotic audio with, by construction,
/// zero packet loss for RTCP to report.
/// Wait for inbound UDP and feed it to str0m. Returns `false` when the loop should exit.
///
/// Split out so the peer-connection lock is released as soon as the receive is done rather
/// than being held for the rest of the iteration.
fn recv_phase(
    handle: &PeerConnectionHandle,
    socket: &UdpSocket,
    recv_buf: &mut [u8],
    wait: Duration,
) -> bool {
    let mut pc = lock_pc(handle);
    if !pc.alive {
        tracing::info!(pc_id = %pc.id, "Peer connection no longer alive");
        return false;
    }
    // A disconnected ICE agent is given time to recover before the loop exits; str0m keeps
    // running connectivity checks throughout, so exiting early is what turns a transient
    // blip into a permanent outage.
    if peer_connection::ice_disconnect_expired(pc.ice_disconnected_since, Instant::now()) {
        tracing::warn!(
            pc_id = %pc.id,
            grace_secs = peer_connection::ICE_DISCONNECT_GRACE.as_secs(),
            "ICE stayed disconnected past the grace period; giving up"
        );
        pc.alive = false;
        return false;
    }
    match peer_connection::recv_and_feed(&mut pc, socket, recv_buf, wait) {
        Ok(true) => peer_connection::drain_backlog(&mut pc, socket, recv_buf),
        Ok(false) => {}
        Err(e) => tracing::debug!("recv_and_feed: {e}"),
    }
    // Released explicitly: the caller's next step is `poll_once`, which takes the same
    // lock, and holding it until the end of scope would serialise the two needlessly.
    drop(pc);
    true
}

#[derive(Default)]
struct WritePacing {
    last_write: Option<Instant>,
    writes: u64,
    burst_writes: u64,
    max_gap_ms: u64,
}

impl WritePacing {
    /// Report every 250 writes: at a nominal 50 frames/sec that is roughly every 5s,
    /// matching the capture side's reporting cadence so the two can be compared directly.
    const REPORT_EVERY: u64 = 250;

    fn record(&mut self, pc_id: &str) {
        let now = Instant::now();
        self.writes = self.writes.saturating_add(1);
        if let Some(previous) = self.last_write {
            let gap = now.saturating_duration_since(previous);
            self.max_gap_ms = self
                .max_gap_ms
                .max(u64::try_from(gap.as_millis()).unwrap_or(u64::MAX));
            if gap < Duration::from_millis(5) {
                self.burst_writes = self.burst_writes.saturating_add(1);
            }
        }
        self.last_write = Some(now);

        if self.writes.is_multiple_of(Self::REPORT_EVERY) {
            tracing::info!(
                pc_id,
                writes = self.writes,
                burst_writes = self.burst_writes,
                max_gap_ms = self.max_gap_ms,
                "Outbound audio wire pacing"
            );
            self.burst_writes = 0;
            self.max_gap_ms = 0;
        }
    }
}

/// Drain any pending I/O commands (non-blocking). Returns `true` if the
/// I/O loop should shut down.
fn drain_io_commands(
    cmd_rx: &mut mpsc::Receiver<IoCommand>,
    handle: &PeerConnectionHandle,
    e2ee: Option<&E2eeContext>,
    pacing: &mut WritePacing,
) -> bool {
    loop {
        match cmd_rx.try_recv() {
            Ok(IoCommand::WriteAudio(opus_data)) => {
                let Some(data) = encrypt_or_drop(e2ee, opus_data, E2eeMediaKind::Audio, "audio")
                else {
                    continue;
                };
                let mut pc = lock_pc(handle);
                pacing.record(&pc.id.clone());
                if let Err(e) = peer_connection::write_audio(&mut pc, &data) {
                    // Throttled warn, not debug: this is the last hop before the network,
                    // so a persistent failure here means nobody hears us -- and at
                    // `debug` it produced a completely clean log while doing so. The
                    // usual causes (no audio mid negotiated, no Opus payload type) are
                    // steady-state, not transient, hence the coarse throttle.
                    static WRITE_FAILURES: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let n = WRITE_FAILURES
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        .saturating_add(1);
                    if n == 1 || n.is_multiple_of(250) {
                        tracing::warn!(
                            pc_id = %pc.id,
                            failures = n,
                            error = %e,
                            "Outbound audio frame not written to the peer connection"
                        );
                    }
                }
            }
            Ok(IoCommand::WriteVideo(vp8_data)) => {
                let Some(data) = encrypt_or_drop(e2ee, vp8_data, E2eeMediaKind::VideoVp8, "video")
                else {
                    continue;
                };
                let mut pc = lock_pc(handle);
                if let Err(e) = peer_connection::write_video(&mut pc, &data) {
                    tracing::debug!("write_video: {e}");
                }
            }
            Ok(IoCommand::Shutdown) => {
                tracing::info!(reason = "shutdown_command", "peer connection closed");
                return true;
            }
            Err(mpsc::error::TryRecvError::Empty) => return false,
            Err(mpsc::error::TryRecvError::Disconnected) => {
                tracing::info!(reason = "command_channel_closed", "peer connection closed");
                return true;
            }
        }
    }
}

/// The blocking I/O loop that drives a single peer connection.
fn io_loop(
    handle: &PeerConnectionHandle,
    socket: &Arc<UdpSocket>,
    mut cmd_rx: mpsc::Receiver<IoCommand>,
    event_tx: &mpsc::Sender<PcEvent>,
    e2ee_ctx: &Arc<Mutex<EncryptionPolicy>>,
) {
    let mut recv_buf = vec![0u8; 2000];
    // Count of `PcEvent`s dropped because `event_tx` (io_loop -> `forward_events`, capacity
    // 256) was full. This is a gap `str0m`'s own `MediaData::contiguous` flag cannot see --
    // `contiguous` reflects RTP-level continuity as decoded by str0m, but a drop here
    // happens strictly after str0m already emitted a complete, contiguous event. If this
    // ever fires, audio/video frames are vanishing between str0m and the playback
    // pipelines without str0m-side loss ever occurring, invisible to the Opus
    // packet-loss-concealment path added for genuine network loss.
    let mut dropped_events: u64 = 0;
    let mut write_pacing = WritePacing::default();

    loop {
        // Snapshot the E2EE policy for this iteration.
        // EncryptionPolicy::clone() is cheap (Arc::clone inside E2eeContext), and this
        // picks up contexts that were initialized after the I/O loop started.
        let e2ee: EncryptionPolicy = e2ee_ctx.lock().map(|g| g.clone()).unwrap_or_default();

        // Process any pending commands (non-blocking)
        if drain_io_commands(&mut cmd_rx, handle, e2ee.as_context(), &mut write_pacing) {
            return;
        }

        // Poll str0m for outputs, decrypt inbound if E2EE is active
        let deadline = {
            let mut pc = lock_pc(handle);
            match peer_connection::poll_once(&mut pc, socket, &mut recv_buf) {
                Ok((events, deadline)) => {
                    for event in events {
                        if let Some(event) = maybe_decrypt_event(event, e2ee.as_context())
                            && event_tx.try_send(event).is_err()
                        {
                            dropped_events = dropped_events.saturating_add(1);
                            // Unthrottled: this is a real invisible-loss channel this
                            // codebase has never had visibility into before, and
                            // capacity-256 means it shouldn't fire under normal load at
                            // all -- if it does, every occurrence matters.
                            tracing::warn!(
                                pc_id = %pc.id,
                                dropped_events,
                                "PcEvent dropped: event_tx to forward_events full"
                            );
                        }
                    }
                    deadline
                }
                Err(e) => {
                    tracing::error!(error = %e, "peer connection failed");
                    return;
                }
            }
        };

        // Wait for UDP data or timeout
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1));
        let wait = wait.min(Duration::from_millis(20)); // Cap at 20ms for responsiveness

        if !recv_phase(handle, socket, &mut recv_buf, wait) {
            return;
        }
    }
}
