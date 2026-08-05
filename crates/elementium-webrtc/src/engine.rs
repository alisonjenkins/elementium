use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use elementium_e2ee::{E2eeContext, MediaKind as E2eeMediaKind};
use elementium_types::VideoFrame;

use crate::peer_connection::{self, PcEvent, PeerConnectionHandle};
use crate::stun;

/// ICE server configuration (STUN/TURN) passed from the signaling layer.
#[derive(Debug, Clone)]
pub struct IceServerConfig {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

/// Command sent to the I/O loop task.
pub enum IoCommand {
    /// Write an Opus frame to the peer connection.
    WriteAudio(Vec<u8>),
    /// Write a VP8 frame to the peer connection.
    WriteVideo(Vec<u8>),
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
    /// Shared E2EE context for frame encryption/decryption.
    /// Uses `Arc<Mutex<>>` so it can be shared with Tauri's `E2eeState` and
    /// populated after I/O loops are already running.
    pub e2ee: Arc<Mutex<Option<E2eeContext>>>,
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
            e2ee: Arc::new(Mutex::new(None)),
        }
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
    ) -> Result<(), String> {
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
        let loop_e2ee = self.e2ee.clone(); // clones the Arc, shares the Option
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

/// Lock the PC handle, recovering from poisoned locks.
///
/// A poisoned lock means a previous holder panicked — we recover the
/// inner data and keep going rather than cascading the panic.
fn lock_pc(
    handle: &PeerConnectionHandle,
) -> std::sync::MutexGuard<'_, peer_connection::PeerConnectionInner> {
    match handle.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!("PC lock was poisoned, recovering");
            poisoned.into_inner()
        }
    }
}

/// E2EE fail-closed: if encryption is configured but fails (no key,
/// poisoned lock, or frame-counter exhaustion), drop the frame rather
/// than sending it in plaintext.
fn encrypt_or_drop(
    e2ee: Option<&E2eeContext>,
    data: Vec<u8>,
    kind: E2eeMediaKind,
    label: &str,
) -> Option<Vec<u8>> {
    let Some(ctx) = e2ee else {
        return Some(data);
    };
    ctx.encrypt_frame(&data, kind).map_or_else(
        || {
            tracing::warn!(
                reason = "e2ee_encrypt_failed",
                label,
                "Dropping outbound frame: E2EE encryption failed"
            );
            None
        },
        Some,
    )
}

/// Drain any pending I/O commands (non-blocking). Returns `true` if the
/// I/O loop should shut down.
fn drain_io_commands(
    cmd_rx: &mut mpsc::Receiver<IoCommand>,
    handle: &PeerConnectionHandle,
    e2ee: Option<&E2eeContext>,
) -> bool {
    loop {
        match cmd_rx.try_recv() {
            Ok(IoCommand::WriteAudio(opus_data)) => {
                let Some(data) = encrypt_or_drop(e2ee, opus_data, E2eeMediaKind::Audio, "audio")
                else {
                    continue;
                };
                let mut pc = lock_pc(handle);
                if let Err(e) = peer_connection::write_audio(&mut pc, &data) {
                    tracing::debug!("write_audio: {e}");
                }
            }
            Ok(IoCommand::WriteVideo(vp8_data)) => {
                let Some(data) = encrypt_or_drop(e2ee, vp8_data, E2eeMediaKind::Video, "video")
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
    e2ee_ctx: &Arc<Mutex<Option<E2eeContext>>>,
) {
    let mut recv_buf = vec![0u8; 2000];

    loop {
        // Snapshot the E2EE context for this iteration.
        // E2eeContext::clone() is cheap (Arc::clone inside), and this picks up
        // contexts that were initialized after the I/O loop started.
        let e2ee: Option<E2eeContext> =
            e2ee_ctx.lock().ok().and_then(|g| g.clone());

        // Process any pending commands (non-blocking)
        if drain_io_commands(&mut cmd_rx, handle, e2ee.as_ref()) {
            return;
        }

        // Poll str0m for outputs, decrypt inbound if E2EE is active
        let deadline = {
            let mut pc = lock_pc(handle);
            match peer_connection::poll_once(&mut pc, socket, &mut recv_buf) {
                Ok((events, deadline)) => {
                    for event in events {
                        let event = maybe_decrypt_event(event, e2ee.as_ref());
                        let _ = event_tx.try_send(event);
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

        {
            let mut pc = lock_pc(handle);
            if !pc.alive {
                tracing::info!(pc_id = %pc.id, "Peer connection no longer alive");
                return;
            }
            if let Err(e) =
                peer_connection::recv_and_feed(&mut pc, socket, &mut recv_buf, wait)
            {
                tracing::debug!("recv_and_feed: {e}");
            }
        }
    }
}

/// Perform STUN discovery using ICE servers and add srflx candidates.
fn discover_and_add_srflx(
    socket: &UdpSocket,
    pc: &mut peer_connection::PeerConnectionInner,
    local_addr: std::net::SocketAddr,
    servers: &[IceServerConfig],
) {
    for server in servers {
        for url in &server.urls {
            if let Some(stun_addr) = stun::parse_stun_url(url) {
                tracing::info!(
                    pc_id = %pc.id,
                    %url,
                    %stun_addr,
                    "Attempting STUN discovery"
                );
                if let Some(srflx_addr) = stun::discover_srflx(socket, stun_addr) {
                    // Use the real local IP as the base (not 0.0.0.0)
                    let base = if local_addr.ip().is_unspecified() {
                        let real_ip = peer_connection::get_local_ip()
                            .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
                        std::net::SocketAddr::new(real_ip, local_addr.port())
                    } else {
                        local_addr
                    };
                    peer_connection::add_srflx_candidate(pc, srflx_addr, base);
                    return; // One srflx candidate is enough
                }
            }
        }
    }
    tracing::warn!(pc_id = %pc.id, "STUN discovery failed on all ICE servers");
}

/// Attempt to decrypt inbound audio/video events if E2EE is active.
///
/// Uses `decrypt_frame_any` which tries all known participant keys, since we
/// don't know which participant sent a particular RTP frame via the SFU.
fn maybe_decrypt_event(event: PcEvent, e2ee: Option<&E2eeContext>) -> PcEvent {
    let Some(ctx) = e2ee else {
        return event;
    };

    match event {
        PcEvent::AudioData(data) => {
            match ctx.decrypt_frame_any(&data, E2eeMediaKind::Audio) {
                Ok(Some(decrypted)) => PcEvent::AudioData(decrypted),
                _ => PcEvent::AudioData(data),
            }
        }
        PcEvent::VideoData(data) => {
            match ctx.decrypt_frame_any(&data, E2eeMediaKind::Video) {
                Ok(Some(decrypted)) => PcEvent::VideoData(decrypted),
                _ => PcEvent::VideoData(data),
            }
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use elementium_e2ee::E2eeOptions;
    use elementium_observability_test::LogCapture;

    use super::*;

    /// Regression test for the fail-open E2EE bug fixed in a prior commit:
    /// when a frame can't be encrypted (e.g. no key set for the local
    /// participant), `encrypt_or_drop` must drop it rather than let it
    /// through as plaintext, and must emit a structured "frame dropped"
    /// warning with a `reason` field so the drop is visible in logs, not
    /// just inferred from an absent return value.
    ///
    /// Manually confirmed this test fails if `encrypt_or_drop`'s `None` arm
    /// is reverted to return `Some(data)` (fail-open) instead of `None`
    /// (fail-closed): the assertion on `result.is_none()` fails immediately.
    #[test]
    // Test assertions are meant to panic on failure; expect() with a
    // descriptive message is the idiomatic way to do that in test code.
    #[allow(clippy::expect_used)]
    fn encrypt_or_drop_emits_structured_warning_when_no_key_set() {
        // No key set for any participant -> encrypt_frame returns None.
        let ctx = E2eeContext::new(E2eeOptions::default());
        let capture = LogCapture::new();

        let result = capture.run(|| {
            encrypt_or_drop(Some(&ctx), b"plaintext-frame".to_vec(), E2eeMediaKind::Audio, "audio")
        });

        // Fail closed: the frame must be dropped, never sent as plaintext.
        assert!(result.is_none());

        let event = capture
            .find_event("Dropping outbound frame")
            .expect("a structured 'frame dropped' warning should have been emitted");
        assert_eq!(event.level, tracing::Level::WARN);
        assert!(event.field("reason").is_some());
        assert_eq!(event.field("label"), Some("audio"));
    }

    /// Sanity check: when no E2EE context is configured at all,
    /// `encrypt_or_drop` passes the frame through unmodified and emits no
    /// drop warning.
    #[test]
    fn encrypt_or_drop_passes_through_when_no_e2ee_configured() {
        let capture = LogCapture::new();
        let result = capture.run(|| {
            encrypt_or_drop(None, b"plaintext-frame".to_vec(), E2eeMediaKind::Audio, "audio")
        });
        assert_eq!(result, Some(b"plaintext-frame".to_vec()));
        assert!(capture.find_event("Dropping outbound frame").is_none());
    }
}
