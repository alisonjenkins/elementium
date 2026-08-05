//! Dual `PeerConnection` transport for `LiveKit` SFU.
//!
//! `LiveKit` uses two `PeerConnections`:
//! - **Publisher**: Client creates offers, sends local audio/video to the SFU.
//! - **Subscriber**: SFU creates offers, sends remote audio/video to the client.
//!
//! Each PC has its own UDP socket and I/O loop, reusing the str0m engine pattern
//! from `engine.rs`.

use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{Instrument, Span};

use elementium_e2ee::{E2eeContext, MediaKind as E2eeMediaKind};
use elementium_types::SessionDescription;

use crate::engine::IceServerConfig;
use crate::peer_connection::{self, PcEvent, PeerConnectionHandle};
use crate::stun;

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

/// Manages the Publisher and Subscriber `PeerConnections` for a `LiveKit` room.
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
    ///
    /// # Errors
    ///
    /// Returns `Err` if either UDP socket fails to bind or its local address
    /// cannot be determined.
    pub fn new(room_id: &str) -> Result<Self, String> {
        Self::new_with_e2ee(room_id, None, None)
    }

    /// Create a new dual-PC transport with optional E2EE and ICE servers.
    ///
    /// # Errors
    ///
    /// Returns `Err` if either UDP socket fails to bind or its local address
    /// cannot be determined.
    pub fn new_with_e2ee(
        room_id: &str,
        e2ee: Option<E2eeContext>,
        ice_servers: Option<&[IceServerConfig]>,
    ) -> Result<Self, String> {
        // Create Publisher PC
        let pub_id = format!("{room_id}-pub");
        let mut pub_inner = peer_connection::create_peer_connection(pub_id);
        let pub_socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| {
            tracing::error!(reason = %e, "transport connect failed");
            format!("Bind pub socket: {e}")
        })?;
        let pub_addr = pub_socket.local_addr().map_err(|e| {
            tracing::error!(reason = %e, "transport connect failed");
            e.to_string()
        })?;
        peer_connection::add_local_candidate(&mut pub_inner, pub_addr);
        // STUN discovery for publisher
        if let Some(servers) = ice_servers {
            discover_and_add_srflx(&pub_socket, &mut pub_inner, pub_addr, servers);
        }
        let pub_handle: PeerConnectionHandle = Arc::new(Mutex::new(pub_inner));
        let pub_socket = Arc::new(pub_socket);

        // Create Subscriber PC
        let sub_id = format!("{room_id}-sub");
        let mut sub_inner = peer_connection::create_peer_connection(sub_id);
        let sub_socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| {
            tracing::error!(reason = %e, "transport connect failed");
            format!("Bind sub socket: {e}")
        })?;
        let sub_addr = sub_socket.local_addr().map_err(|e| {
            tracing::error!(reason = %e, "transport connect failed");
            e.to_string()
        })?;
        peer_connection::add_local_candidate(&mut sub_inner, sub_addr);
        // STUN discovery for subscriber
        if let Some(servers) = ice_servers {
            discover_and_add_srflx(&sub_socket, &mut sub_inner, sub_addr, servers);
        }
        let sub_handle: PeerConnectionHandle = Arc::new(Mutex::new(sub_inner));
        let sub_socket = Arc::new(sub_socket);

        // Transport command/event channels
        let (cmd_tx, cmd_rx) = mpsc::channel::<TransportCommand>(256);
        let (event_tx, event_rx) = mpsc::channel::<TransportEvent>(256);

        // Internal channels for per-PC events
        let (pub_event_tx, pub_event_rx) = mpsc::channel::<PcEvent>(256);
        let (sub_event_tx, sub_event_rx) = mpsc::channel::<PcEvent>(256);
        let (pub_cmd_tx, pub_cmd_rx) = mpsc::channel::<PcCommand>(256);

        // spawn_blocking doesn't propagate the ambient span on its own, so capture it
        // here and enter it inside each closure to keep the same correlation_id.
        let io_loop_span = Span::current();

        // Spawn Publisher I/O loop
        let pub_h = pub_handle.clone();
        let pub_s = pub_socket.clone();
        let pub_e2ee = e2ee.clone();
        let pub_span = io_loop_span.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = pub_span.enter();
            pc_io_loop(pub_h, pub_s, Some(pub_cmd_rx), pub_event_tx, pub_e2ee);
        });

        // Spawn Subscriber I/O loop
        let sub_h = sub_handle.clone();
        let sub_s = sub_socket.clone();
        let sub_e2ee = e2ee.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = io_loop_span.enter();
            pc_io_loop(sub_h, sub_s, None, sub_event_tx, sub_e2ee);
        });

        // Spawn dispatcher: routes TransportCommands to Publisher and merges events
        tokio::spawn(
            transport_dispatch(cmd_rx, pub_cmd_tx, pub_event_rx, sub_event_rx, event_tx)
                .instrument(Span::current()),
        );

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
    ///
    /// # Errors
    ///
    /// Returns `Err` if the Publisher PC lock is poisoned or offer creation fails.
    pub fn create_publisher_offer(
        &self,
        include_video: bool,
    ) -> Result<SessionDescription, String> {
        let mut pc = self.publisher.lock().map_err(|e| e.to_string())?;
        let mut transceivers = vec![peer_connection::TransceiverInfo {
            kind: str0m::media::MediaKind::Audio,
            direction: str0m::media::Direction::SendRecv,
        }];
        if include_video {
            transceivers.push(peer_connection::TransceiverInfo {
                kind: str0m::media::MediaKind::Video,
                direction: str0m::media::Direction::SendRecv,
            });
        }
        peer_connection::create_offer(&mut pc, &[], &transceivers)
    }

    /// Set the SDP answer on the Publisher PC (received from SFU).
    ///
    /// # Errors
    ///
    /// Returns `Err` if the Publisher PC lock is poisoned or applying the
    /// remote description fails.
    pub fn set_publisher_answer(&self, answer: &SessionDescription) -> Result<(), String> {
        let mut pc = self.publisher.lock().map_err(|e| e.to_string())?;
        peer_connection::set_remote_description(&mut pc, answer)?;
        Ok(())
    }

    /// Set the SDP offer on the Subscriber PC (from SFU) and return the answer.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the Subscriber PC lock is poisoned, applying the
    /// remote description fails, or no answer is produced.
    pub fn set_subscriber_offer(
        &self,
        offer: &SessionDescription,
    ) -> Result<SessionDescription, String> {
        let mut guard = self.subscriber.lock().map_err(|e| e.to_string())?;
        let answer = peer_connection::set_remote_description(&mut guard, offer)?;
        drop(guard);
        answer.ok_or_else(|| "Expected answer from subscriber offer".into())
    }

    /// Add an ICE candidate to the correct PC based on target.
    /// target=0 → Publisher, target=1 → Subscriber.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the target PC lock is poisoned or the candidate is invalid.
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
    ///
    /// # Errors
    ///
    /// Returns `Err` if the transport command channel has been closed.
    pub async fn send_command(&self, cmd: TransportCommand) -> Result<(), String> {
        self.cmd_tx
            .send(cmd)
            .await
            .map_err(|_| "Transport command channel closed".to_string())
    }

    /// Shut down both `PeerConnections`.
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

/// Blocking I/O loop for a single `PeerConnection`.
// `handle`/`socket`/`event_tx`/`e2ee` must be owned: this function runs for the
// lifetime of a dedicated `spawn_blocking` thread (see call sites in `new_with_e2ee`),
// which requires `'static` captures, so references are not viable here.
#[allow(clippy::needless_pass_by_value)]
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
                        // E2EE fail-closed: if encryption is configured but fails (no key,
                        // poisoned lock, or frame-counter exhaustion), drop the frame rather
                        // than sending it in plaintext.
                        let data = if let Some(ctx) = &e2ee {
                            if let Some(encrypted) =
                                ctx.encrypt_frame(&data, E2eeMediaKind::Audio)
                            {
                                encrypted
                            } else {
                                tracing::warn!(
                                    "Dropping outbound audio frame: E2EE encryption failed"
                                );
                                continue;
                            }
                        } else {
                            data
                        };
                        let mut pc = lock_pc!(handle);
                        if let Err(e) = peer_connection::write_audio(&mut pc, &data) {
                            tracing::debug!(reason = %e, "write_audio failed");
                        }
                    }
                    Ok(PcCommand::WriteVideo(data)) => {
                        // E2EE fail-closed: see WriteAudio above.
                        let data = if let Some(ctx) = &e2ee {
                            if let Some(encrypted) =
                                ctx.encrypt_frame(&data, E2eeMediaKind::Video)
                            {
                                encrypted
                            } else {
                                tracing::warn!(
                                    "Dropping outbound video frame: E2EE encryption failed"
                                );
                                continue;
                            }
                        } else {
                            data
                        };
                        let mut pc = lock_pc!(handle);
                        if let Err(e) = peer_connection::write_video(&mut pc, &data) {
                            tracing::debug!(reason = %e, "write_video failed");
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
                        let event = maybe_decrypt_event(event, e2ee.as_ref());
                        let _ = event_tx.try_send(event);
                    }
                    deadline
                }
                Err(e) => {
                    tracing::error!(reason = %e, "poll_once failed");
                    return;
                }
            }
        };

        let wait = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1));
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
                tracing::debug!(reason = %e, "recv_and_feed failed");
            }
        }
    }
}

/// Attempt to decrypt inbound audio/video events if E2EE is active.
fn maybe_decrypt_event(event: PcEvent, e2ee: Option<&E2eeContext>) -> PcEvent {
    let Some(ctx) = e2ee else {
        return event;
    };

    match event {
        PcEvent::AudioData(data) => match ctx.decrypt_frame(&data, "", E2eeMediaKind::Audio) {
            Ok(Some(decrypted)) => PcEvent::AudioData(decrypted),
            _ => PcEvent::AudioData(data),
        },
        PcEvent::VideoData(data) => match ctx.decrypt_frame(&data, "", E2eeMediaKind::Video) {
            Ok(Some(decrypted)) => PcEvent::VideoData(decrypted),
            _ => PcEvent::VideoData(data),
        },
        other => other,
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
                    "Attempting STUN discovery (transport)"
                );
                if let Some(srflx_addr) = stun::discover_srflx(socket, stun_addr) {
                    let base = if local_addr.ip().is_unspecified() {
                        let real_ip = peer_connection::get_local_ip()
                            .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
                        std::net::SocketAddr::new(real_ip, local_addr.port())
                    } else {
                        local_addr
                    };
                    peer_connection::add_srflx_candidate(pc, srflx_addr, base);
                    return;
                }
            }
        }
    }
    tracing::warn!(pc_id = %pc.id, "STUN discovery failed on all ICE servers (transport)");
}

/// Async dispatcher: routes `TransportCommands` to the Publisher and merges PC events.
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
