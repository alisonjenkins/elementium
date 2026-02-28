//! Dual PeerConnection transport for LiveKit SFU.
//!
//! LiveKit uses two PeerConnections:
//! - **Publisher**: Client creates offers, sends local audio/video to the SFU.
//! - **Subscriber**: SFU creates offers, sends remote audio/video to the client.
//!
//! Each PC has its own UDP socket and I/O loop, reusing the str0m engine pattern
//! from `engine.rs`.

use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use elementium_e2ee::{E2eeContext, MediaKind as E2eeMediaKind};
use elementium_types::SessionDescription;

use crate::peer_connection::{self, PcEvent, PeerConnectionHandle};

/// Events from the transport layer to the room.
#[derive(Debug)]
pub enum TransportEvent {
    /// Publisher PC event.
    PublisherEvent(PcEvent),
    /// Subscriber PC event.
    SubscriberEvent(PcEvent),
}

/// Commands to the transport I/O loops.
pub enum TransportCommand {
    /// Write an Opus audio frame to the Publisher PC.
    WriteAudio(Vec<u8>),
    /// Write a VP8 video frame to the Publisher PC.
    WriteVideo(Vec<u8>),
    /// Shut down the transport.
    Shutdown,
}

/// Manages the Publisher and Subscriber PeerConnections for a LiveKit room.
pub struct Transport {
    pub publisher: PeerConnectionHandle,
    pub subscriber: PeerConnectionHandle,
    pub pub_socket: Arc<UdpSocket>,
    pub sub_socket: Arc<UdpSocket>,
    pub cmd_tx: mpsc::Sender<TransportCommand>,
    pub event_rx: Arc<Mutex<mpsc::Receiver<TransportEvent>>>,
    pub e2ee: Option<E2eeContext>,
}

impl Transport {
    /// Create a new dual-PC transport. Binds two UDP sockets and starts I/O loops.
    pub fn new(room_id: &str) -> Result<Self, String> {
        Self::new_with_e2ee(room_id, None)
    }

    /// Create a new dual-PC transport with optional E2EE.
    pub fn new_with_e2ee(room_id: &str, e2ee: Option<E2eeContext>) -> Result<Self, String> {
        // Create Publisher PC
        let pub_id = format!("{room_id}-pub");
        let mut pub_inner = peer_connection::create_peer_connection(pub_id);
        let pub_socket =
            UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("Bind pub socket: {e}"))?;
        let pub_addr = pub_socket.local_addr().map_err(|e| e.to_string())?;
        peer_connection::add_local_candidate(&mut pub_inner, pub_addr);
        let pub_handle: PeerConnectionHandle = Arc::new(Mutex::new(pub_inner));
        let pub_socket = Arc::new(pub_socket);

        // Create Subscriber PC
        let sub_id = format!("{room_id}-sub");
        let mut sub_inner = peer_connection::create_peer_connection(sub_id);
        let sub_socket =
            UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("Bind sub socket: {e}"))?;
        let sub_addr = sub_socket.local_addr().map_err(|e| e.to_string())?;
        peer_connection::add_local_candidate(&mut sub_inner, sub_addr);
        let sub_handle: PeerConnectionHandle = Arc::new(Mutex::new(sub_inner));
        let sub_socket = Arc::new(sub_socket);

        // Transport command/event channels
        let (cmd_tx, cmd_rx) = mpsc::channel::<TransportCommand>(256);
        let (event_tx, event_rx) = mpsc::channel::<TransportEvent>(256);

        // Internal channels for per-PC events
        let (pub_event_tx, pub_event_rx) = mpsc::channel::<PcEvent>(256);
        let (sub_event_tx, sub_event_rx) = mpsc::channel::<PcEvent>(256);
        let (pub_cmd_tx, pub_cmd_rx) = mpsc::channel::<PcCommand>(256);

        // Spawn Publisher I/O loop
        let pub_h = pub_handle.clone();
        let pub_s = pub_socket.clone();
        let pub_e2ee = e2ee.clone();
        tokio::task::spawn_blocking(move || {
            pc_io_loop(pub_h, pub_s, Some(pub_cmd_rx), pub_event_tx, pub_e2ee);
        });

        // Spawn Subscriber I/O loop
        let sub_h = sub_handle.clone();
        let sub_s = sub_socket.clone();
        let sub_e2ee = e2ee.clone();
        tokio::task::spawn_blocking(move || {
            pc_io_loop(sub_h, sub_s, None, sub_event_tx, sub_e2ee);
        });

        // Spawn dispatcher: routes TransportCommands to Publisher and merges events
        tokio::spawn(transport_dispatch(
            cmd_rx,
            pub_cmd_tx,
            pub_event_rx,
            sub_event_rx,
            event_tx,
        ));

        Ok(Self {
            publisher: pub_handle,
            subscriber: sub_handle,
            pub_socket,
            sub_socket,
            cmd_tx,
            event_rx: Arc::new(Mutex::new(event_rx)),
            e2ee,
        })
    }

    /// Create an SDP offer on the Publisher PC (for publishing tracks).
    pub fn create_publisher_offer(
        &self,
        include_video: bool,
    ) -> Result<SessionDescription, String> {
        let mut pc = self.publisher.lock().map_err(|e| e.to_string())?;
        peer_connection::create_offer(&mut pc, include_video)
    }

    /// Set the SDP answer on the Publisher PC (received from SFU).
    pub fn set_publisher_answer(&self, answer: &SessionDescription) -> Result<(), String> {
        let mut pc = self.publisher.lock().map_err(|e| e.to_string())?;
        peer_connection::set_remote_description(&mut pc, answer)?;
        Ok(())
    }

    /// Set the SDP offer on the Subscriber PC (from SFU) and return the answer.
    pub fn set_subscriber_offer(
        &self,
        offer: &SessionDescription,
    ) -> Result<SessionDescription, String> {
        let mut pc = self.subscriber.lock().map_err(|e| e.to_string())?;
        let answer = peer_connection::set_remote_description(&mut pc, offer)?;
        answer.ok_or_else(|| "Expected answer from subscriber offer".into())
    }

    /// Add an ICE candidate to the correct PC based on target.
    /// target=0 → Publisher, target=1 → Subscriber.
    pub fn add_ice_candidate(&self, target: i32, candidate_sdp: &str) -> Result<(), String> {
        let handle = if target == 0 {
            &self.publisher
        } else {
            &self.subscriber
        };
        let mut pc = handle.lock().map_err(|e| e.to_string())?;
        peer_connection::add_ice_candidate(&mut pc, candidate_sdp)
    }

    /// Send a command to write audio/video or shutdown.
    pub async fn send_command(&self, cmd: TransportCommand) -> Result<(), String> {
        self.cmd_tx
            .send(cmd)
            .await
            .map_err(|_| "Transport command channel closed".to_string())
    }

    /// Shut down both PeerConnections.
    pub async fn shutdown(&self) {
        let _ = self.cmd_tx.send(TransportCommand::Shutdown).await;
    }
}

/// Internal command for the Publisher I/O loop.
enum PcCommand {
    WriteAudio(Vec<u8>),
    WriteVideo(Vec<u8>),
    Shutdown,
}

/// Blocking I/O loop for a single PeerConnection.
fn pc_io_loop(
    handle: PeerConnectionHandle,
    socket: Arc<UdpSocket>,
    cmd_rx: Option<mpsc::Receiver<PcCommand>>,
    event_tx: mpsc::Sender<PcEvent>,
    e2ee: Option<E2eeContext>,
) {
    let mut recv_buf = vec![0u8; 2000];
    let mut cmd_rx = cmd_rx;

    // Helper: lock the PC handle, recovering from poisoned locks.
    macro_rules! lock_pc {
        ($handle:expr) => {
            match $handle.lock() {
                Ok(guard) => guard,
                Err(poisoned) => {
                    tracing::warn!("Transport PC lock was poisoned, recovering");
                    poisoned.into_inner()
                }
            }
        };
    }

    loop {
        // Process commands (only Publisher has commands)
        if let Some(ref mut rx) = cmd_rx {
            loop {
                match rx.try_recv() {
                    Ok(PcCommand::WriteAudio(data)) => {
                        let data = match &e2ee {
                            Some(ctx) => ctx
                                .encrypt_frame(&data, E2eeMediaKind::Audio)
                                .unwrap_or(data),
                            None => data,
                        };
                        let mut pc = lock_pc!(handle);
                        if let Err(e) = peer_connection::write_audio(&mut pc, &data) {
                            tracing::debug!("write_audio: {e}");
                        }
                    }
                    Ok(PcCommand::WriteVideo(data)) => {
                        let data = match &e2ee {
                            Some(ctx) => ctx
                                .encrypt_frame(&data, E2eeMediaKind::Video)
                                .unwrap_or(data),
                            None => data,
                        };
                        let mut pc = lock_pc!(handle);
                        if let Err(e) = peer_connection::write_video(&mut pc, &data) {
                            tracing::debug!("write_video: {e}");
                        }
                    }
                    Ok(PcCommand::Shutdown) => {
                        tracing::info!("Transport PC I/O loop shutting down");
                        return;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => return,
                }
            }
        }

        // Poll str0m
        let deadline = {
            let mut pc = lock_pc!(handle);
            match peer_connection::poll_once(&mut pc, &socket, &mut recv_buf) {
                Ok((events, deadline)) => {
                    for event in events {
                        let event = maybe_decrypt_event(event, &e2ee);
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

        let wait = (deadline - Instant::now()).max(Duration::from_millis(1));
        let wait = wait.min(Duration::from_millis(20));

        {
            let mut pc = lock_pc!(handle);
            if !pc.alive {
                tracing::info!(pc_id = %pc.id, "Transport PC no longer alive");
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

/// Attempt to decrypt inbound audio/video events if E2EE is active.
fn maybe_decrypt_event(event: PcEvent, e2ee: &Option<E2eeContext>) -> PcEvent {
    let Some(ctx) = e2ee else {
        return event;
    };

    match event {
        PcEvent::AudioData(data) => {
            match ctx.decrypt_frame(&data, "", E2eeMediaKind::Audio) {
                Ok(Some(decrypted)) => PcEvent::AudioData(decrypted),
                _ => PcEvent::AudioData(data),
            }
        }
        PcEvent::VideoData(data) => {
            match ctx.decrypt_frame(&data, "", E2eeMediaKind::Video) {
                Ok(Some(decrypted)) => PcEvent::VideoData(decrypted),
                _ => PcEvent::VideoData(data),
            }
        }
        other => other,
    }
}

/// Async dispatcher: routes TransportCommands to the Publisher and merges PC events.
async fn transport_dispatch(
    mut cmd_rx: mpsc::Receiver<TransportCommand>,
    pub_cmd_tx: mpsc::Sender<PcCommand>,
    mut pub_event_rx: mpsc::Receiver<PcEvent>,
    mut sub_event_rx: mpsc::Receiver<PcEvent>,
    event_tx: mpsc::Sender<TransportEvent>,
) {
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(TransportCommand::WriteAudio(data)) => {
                        let _ = pub_cmd_tx.send(PcCommand::WriteAudio(data)).await;
                    }
                    Some(TransportCommand::WriteVideo(data)) => {
                        let _ = pub_cmd_tx.send(PcCommand::WriteVideo(data)).await;
                    }
                    Some(TransportCommand::Shutdown) => {
                        let _ = pub_cmd_tx.send(PcCommand::Shutdown).await;
                        break;
                    }
                    None => break,
                }
            }
            ev = pub_event_rx.recv() => {
                if let Some(ev) = ev {
                    let _ = event_tx.send(TransportEvent::PublisherEvent(ev)).await;
                }
            }
            ev = sub_event_rx.recv() => {
                if let Some(ev) = ev {
                    let _ = event_tx.send(TransportEvent::SubscriberEvent(ev)).await;
                }
            }
        }
    }
    tracing::info!("Transport dispatch ended");
}
