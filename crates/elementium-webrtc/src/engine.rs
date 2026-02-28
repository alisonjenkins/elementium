use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use elementium_types::VideoFrame;

use crate::peer_connection::{self, PcEvent, PeerConnectionHandle};

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
}

/// The WebRTC engine manages all active peer connections.
pub struct WebRtcEngine {
    connections: HashMap<String, ManagedPc>,
    /// Shared video frame buffer for all connections.
    pub video_frames: VideoFrameBuffer,
}

impl WebRtcEngine {
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
            video_frames: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Create a new peer connection. Binds a UDP socket and starts the I/O loop.
    pub fn create_connection(&mut self, id: String) -> Result<(), String> {
        let mut pc_inner = peer_connection::create_peer_connection(id.clone());

        // Bind a UDP socket for this connection
        let socket =
            UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("Failed to bind socket: {e}"))?;
        let local_addr = socket.local_addr().map_err(|e| e.to_string())?;

        // Add the socket address as a local ICE candidate
        peer_connection::add_local_candidate(&mut pc_inner, local_addr);

        let handle: PeerConnectionHandle = Arc::new(Mutex::new(pc_inner));
        let socket = Arc::new(socket);

        // Channels for the I/O loop
        let (io_cmd_tx, io_cmd_rx) = mpsc::channel::<IoCommand>(256);
        let (event_tx, event_rx) = mpsc::channel::<PcEvent>(256);

        // Spawn the I/O loop as a blocking task (it does synchronous UDP I/O)
        let loop_handle = handle.clone();
        let loop_socket = socket.clone();
        tokio::task::spawn_blocking(move || {
            io_loop(loop_handle, loop_socket, io_cmd_rx, event_tx);
        });

        self.connections.insert(
            id,
            ManagedPc {
                handle,
                socket,
                io_cmd_tx,
                event_rx: Arc::new(Mutex::new(event_rx)),
            },
        );

        Ok(())
    }

    /// Get a reference to a managed peer connection.
    pub fn get(&self, id: &str) -> Option<&ManagedPc> {
        self.connections.get(id)
    }

    /// Remove and shut down a peer connection.
    pub fn remove(&mut self, id: &str) -> Option<ManagedPc> {
        if let Some(managed) = self.connections.remove(id) {
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
    pub fn connection_ids(&self) -> Vec<String> {
        self.connections.keys().cloned().collect()
    }
}

/// The blocking I/O loop that drives a single peer connection.
fn io_loop(
    handle: PeerConnectionHandle,
    socket: Arc<UdpSocket>,
    mut cmd_rx: mpsc::Receiver<IoCommand>,
    event_tx: mpsc::Sender<PcEvent>,
) {
    let mut recv_buf = vec![0u8; 2000];

    loop {
        // Process any pending commands (non-blocking)
        loop {
            match cmd_rx.try_recv() {
                Ok(IoCommand::WriteAudio(opus_data)) => {
                    let mut pc = handle.lock().unwrap();
                    if let Err(e) = peer_connection::write_audio(&mut pc, &opus_data) {
                        tracing::debug!("write_audio: {e}");
                    }
                }
                Ok(IoCommand::WriteVideo(vp8_data)) => {
                    let mut pc = handle.lock().unwrap();
                    if let Err(e) = peer_connection::write_video(&mut pc, &vp8_data) {
                        tracing::debug!("write_video: {e}");
                    }
                }
                Ok(IoCommand::Shutdown) => {
                    tracing::info!("I/O loop shutting down");
                    return;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    tracing::info!("I/O loop command channel closed");
                    return;
                }
            }
        }

        // Poll str0m for outputs
        let deadline = {
            let mut pc = handle.lock().unwrap();
            match peer_connection::poll_once(&mut pc, &socket, &mut recv_buf) {
                Ok((events, deadline)) => {
                    for event in events {
                        let _ = event_tx.try_send(event);
                    }
                    deadline
                }
                Err(e) => {
                    tracing::error!("poll_once error: {e}");
                    return;
                }
            }
        };

        // Wait for UDP data or timeout
        let wait = (deadline - Instant::now()).max(Duration::from_millis(1));
        let wait = wait.min(Duration::from_millis(20)); // Cap at 20ms for responsiveness

        {
            let mut pc = handle.lock().unwrap();
            if !pc.alive {
                tracing::info!(pc_id = %pc.id, "Peer connection no longer alive");
                return;
            }
            if let Err(e) =
                peer_connection::recv_and_feed(&mut pc, &socket, &mut recv_buf, wait)
            {
                tracing::debug!("recv_and_feed: {e}");
            }
        }
    }
}
