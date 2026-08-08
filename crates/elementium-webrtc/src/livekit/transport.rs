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

use crate::e2ee_io::EncryptionPolicy;

use crate::engine::IceServerConfig;
use crate::peer_connection::{
    self, PcEvent, PeerConnectionHandle, discover_and_add_srflx, lock_pc,
};

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
    /// Write an encoded Opus audio frame to the Publisher PC. Carries
    /// [`elementium_types::PlaintextMedia`] so it cannot reach the socket without
    /// passing through encryption first.
    WriteAudio(elementium_types::PlaintextMedia),
    /// Write an encoded video frame to the Publisher PC, in the codec named alongside it.
    /// See [`TransportCommand::WriteAudio`] and [`crate::engine::IoCommand::WriteVideo`].
    WriteVideo(
        elementium_types::PlaintextMedia,
        elementium_codec::VideoCodec,
    ),
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
    pub e2ee: EncryptionPolicy,
    /// The subscriber I/O loop's command channel, kept only so it can be closed.
    ///
    /// Nothing is ever sent on it -- a subscriber has no media to write. It exists
    /// because a `spawn_blocking` task that never returns is one the tokio runtime can
    /// never shut down, so *something* has to tell that loop its transport is gone.
    /// Dropping this sender does it: the loop sees `Disconnected` and returns.
    sub_cmd_tx: mpsc::Sender<PcCommand>,
}

impl Transport {
    /// Create a new dual-PC transport with an E2EE policy and optional ICE servers.
    ///
    /// # Errors
    ///
    /// Returns `Err` if either UDP socket fails to bind or its local address
    /// cannot be determined.
    pub fn new_with_e2ee(
        room_id: &str,
        e2ee: EncryptionPolicy,
        ice_servers: Option<&[IceServerConfig]>,
    ) -> Result<Self, crate::error::WebRtcError> {
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
        // See `Transport::sub_cmd_tx`: the subscriber's only use for a command channel is
        // learning when to stop.
        let (sub_cmd_tx, sub_cmd_rx) = mpsc::channel::<PcCommand>(1);

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
            pc_io_loop(sub_h, sub_s, Some(sub_cmd_rx), sub_event_tx, sub_e2ee);
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
            sub_cmd_tx,
        })
    }

    /// Create an SDP offer on the Publisher PC (for publishing tracks).
    ///
    /// # Errors
    ///
    /// Returns `Err` if the Publisher PC lock is poisoned or offer creation fails.
    /// Create the publisher offer.
    ///
    /// `audio_cid`/`video_cid` are the `cid` values sent in the matching `AddTrackRequest`,
    /// and become the offer's msid track ids. The SFU pairs a published track with an
    /// m-line by matching those two, so they must agree -- see
    /// [`peer_connection::TransceiverInfo::track_id`].
    pub fn create_publisher_offer(
        &self,
        include_video: bool,
        audio_cid: Option<String>,
        video_cid: Option<String>,
    ) -> Result<SessionDescription, crate::error::WebRtcError> {
        let mut pc = self
            .publisher
            .lock()
            .map_err(|_| crate::error::WebRtcError::LockPoisoned)?;
        let mut transceivers = vec![peer_connection::TransceiverInfo {
            kind: str0m::media::MediaKind::Audio,
            direction: str0m::media::Direction::SendRecv,
            track_id: audio_cid,
        }];
        if include_video {
            transceivers.push(peer_connection::TransceiverInfo {
                kind: str0m::media::MediaKind::Video,
                direction: str0m::media::Direction::SendRecv,
                track_id: video_cid,
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
    pub fn set_publisher_answer(
        &self,
        answer: &SessionDescription,
    ) -> Result<(), crate::error::WebRtcError> {
        let mut pc = self
            .publisher
            .lock()
            .map_err(|_| crate::error::WebRtcError::LockPoisoned)?;
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
    ) -> Result<SessionDescription, crate::error::WebRtcError> {
        let mut guard = self
            .subscriber
            .lock()
            .map_err(|_| crate::error::WebRtcError::LockPoisoned)?;
        let answer = peer_connection::set_remote_description(&mut guard, offer)?;
        drop(guard);
        answer.ok_or_else(|| {
            crate::error::WebRtcError::Sdp("expected answer from subscriber offer".to_string())
        })
    }

    /// Send a command to write audio/video or shutdown.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the transport command channel has been closed.
    pub async fn send_command(
        &self,
        cmd: TransportCommand,
    ) -> Result<(), crate::error::WebRtcError> {
        self.cmd_tx
            .send(cmd)
            .await
            .map_err(|_| crate::error::WebRtcError::ChannelClosed("transport command"))
    }

    /// Shut down both `PeerConnections`.
    ///
    /// Both, not just the publisher: the dispatcher only reaches the publisher, and a
    /// subscriber loop left running holds a `spawn_blocking` thread open for good.
    pub async fn shutdown(&self) {
        let _ = self.cmd_tx.send(TransportCommand::Shutdown).await;
        let _ = self.sub_cmd_tx.send(PcCommand::Shutdown).await;
    }
}

/// What a write is, carrying whatever the rest of the path needs to know about it.
///
/// The video codec travels here rather than being assumed, because two decisions downstream
/// depend on it: the RTP payload type, and how much of the frame E2EE leaves in the clear.
#[derive(Clone, Copy)]
enum WriteKind {
    Audio,
    Video(elementium_codec::VideoCodec),
}

/// Internal command for the Publisher I/O loop.
enum PcCommand {
    WriteAudio(elementium_types::PlaintextMedia),
    WriteVideo(
        elementium_types::PlaintextMedia,
        elementium_codec::VideoCodec,
    ),
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
    e2ee: EncryptionPolicy,
) {
    let mut recv_buf = vec![0u8; 2000];
    let mut cmd_rx = cmd_rx;
    tracing::info!(
        e2ee_active = e2ee.as_context().is_some(),
        "PC I/O loop starting"
    );
    let mut inbound_audio_count: u64 = 0;
    let mut audio_publish = PublishCounters::default();
    let mut video_publish = PublishCounters::default();
    // See `engine::io_loop`'s `dropped_events` -- same invisible-loss blind spot on the
    // LiveKit subscriber path's own event channel to `room.rs`.
    let mut dropped_events: u64 = 0;

    loop {
        // Process commands (only Publisher has commands)
        if let Some(ref mut rx) = cmd_rx {
            loop {
                match rx.try_recv() {
                    Ok(PcCommand::WriteAudio(data)) => {
                        write_encrypted_or_drop(
                            &handle,
                            e2ee.as_context(),
                            data,
                            WriteKind::Audio,
                            &mut audio_publish,
                        );
                    }
                    Ok(PcCommand::WriteVideo(data, codec)) => {
                        write_encrypted_or_drop(
                            &handle,
                            e2ee.as_context(),
                            data,
                            WriteKind::Video(codec),
                            &mut video_publish,
                        );
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
            let mut pc = lock_pc(&handle);
            match peer_connection::poll_once(&mut pc, &socket, &mut recv_buf) {
                Ok((events, deadline)) => {
                    for event in events {
                        if let Some(event) = log_and_decrypt_event(
                            event,
                            e2ee.as_context(),
                            &mut inbound_audio_count,
                        ) && event_tx.try_send(event).is_err()
                        {
                            dropped_events = dropped_events.saturating_add(1);
                            tracing::warn!(
                                dropped_events,
                                "PcEvent dropped: event_tx to room.rs full"
                            );
                        }
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
            let mut pc = lock_pc(&handle);
            if !pc.alive {
                tracing::info!(pc_id = %pc.id, "Transport PC no longer alive");
                return;
            }
            match peer_connection::recv_and_feed(&mut pc, &socket, &mut recv_buf, wait) {
                Ok(true) => peer_connection::drain_backlog(&mut pc, &socket, &mut recv_buf),
                Ok(false) => {}
                Err(e) => {
                    tracing::debug!(reason = %e, "recv_and_feed failed");
                }
            }
        }
    }
}

/// What happened to the frames we tried to publish, per media kind.
///
/// Every stage between "the encoder produced a frame" and "str0m accepted it" can fail
/// silently and independently: encryption drops a frame when no key is set, and the write
/// fails when no mid or payload type has been negotiated yet. Each was a `debug!` on the
/// individual frame, which at 50 frames a second is unreadable when it fires and
/// indistinguishable from success when it does not. Counting them makes "the far end
/// hears nothing" a question with an answer.
#[derive(Default)]
struct PublishCounters {
    offered: u64,
    encrypt_dropped: u64,
    written: u64,
    write_failed: u64,
    last_failure: Option<String>,
}

impl PublishCounters {
    /// Report every this many offered frames -- one second of audio, ~2s of video.
    const REPORT_EVERY: u64 = 50;

    fn report(&self, kind: &str) {
        if self.written == 0 {
            tracing::warn!(
                kind,
                offered = self.offered,
                encrypt_dropped = self.encrypt_dropped,
                write_failed = self.write_failed,
                last_failure = self.last_failure.as_deref().unwrap_or("none"),
                "publishing produced no frames on the wire"
            );
        } else {
            tracing::info!(
                kind,
                offered = self.offered,
                written = self.written,
                encrypt_dropped = self.encrypt_dropped,
                write_failed = self.write_failed,
                last_failure = self.last_failure.as_deref().unwrap_or("none"),
                "publishing"
            );
        }
    }
}

/// Encrypt an outbound audio/video frame (if E2EE is active, via the shared
/// [`crate::e2ee_io::encrypt_or_drop`]) and write it to the peer connection.
fn write_encrypted_or_drop(
    handle: &PeerConnectionHandle,
    e2ee: Option<&E2eeContext>,
    data: elementium_types::PlaintextMedia,
    what: WriteKind,
    counters: &mut PublishCounters,
) {
    let label = match what {
        WriteKind::Audio => "audio",
        WriteKind::Video(_) => "video",
    };
    counters.offered = counters.offered.saturating_add(1);

    let kind = match what {
        WriteKind::Audio => E2eeMediaKind::Audio,
        WriteKind::Video(codec) => {
            let Some(kind) = crate::e2ee_io::video_media_kind(codec) else {
                counters.encrypt_dropped = counters.encrypt_dropped.saturating_add(1);
                counters.last_failure = Some(format!(
                    "{} has no E2EE framing a peer could undo",
                    codec.sdp_name()
                ));
                return;
            };
            kind
        }
    };

    let Some(data) = crate::e2ee_io::encrypt_or_drop(e2ee, data, kind, label) else {
        counters.encrypt_dropped = counters.encrypt_dropped.saturating_add(1);
        counters.last_failure = Some("e2ee encryption failed".to_owned());
        if counters
            .offered
            .is_multiple_of(PublishCounters::REPORT_EVERY)
        {
            counters.report(label);
        }
        return;
    };

    let result = {
        let mut pc = lock_pc(handle);
        match what {
            WriteKind::Audio => peer_connection::write_audio(&mut pc, &data),
            WriteKind::Video(codec) => peer_connection::write_video(&mut pc, &data, codec),
        }
    };
    match result {
        Ok(()) => counters.written = counters.written.saturating_add(1),
        Err(e) => {
            counters.write_failed = counters.write_failed.saturating_add(1);
            counters.last_failure = Some(e.to_string());
        }
    }

    if counters
        .offered
        .is_multiple_of(PublishCounters::REPORT_EVERY)
    {
        counters.report(label);
    }
}

/// Log throttled diagnostics for inbound audio events (every 100th frame), then delegate to
/// [`maybe_decrypt_event`]. Split out from `pc_io_loop` to keep that function under clippy's
/// line-count limit.
fn log_and_decrypt_event(
    event: crate::peer_connection::WirePcEvent,
    e2ee: Option<&E2eeContext>,
    inbound_audio_count: &mut u64,
) -> Option<PcEvent> {
    if let PcEvent::AudioData { ref data, .. } = event {
        *inbound_audio_count = inbound_audio_count.saturating_add(1);
        if inbound_audio_count.is_multiple_of(100) {
            tracing::info!(
                count = *inbound_audio_count,
                encrypted_len = data.len(),
                e2ee_active = e2ee.is_some(),
                "Inbound audio RTP payload received"
            );
        }
    }
    let decrypted = crate::e2ee_io::maybe_decrypt_event(event, e2ee);
    if let Some(PcEvent::AudioData { ref data, .. }) = decrypted
        && inbound_audio_count.is_multiple_of(100)
    {
        tracing::info!(
            count = *inbound_audio_count,
            decrypted_len = data.len(),
            "Inbound audio payload after E2EE step"
        );
    }
    decrypted
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
                    Some(TransportCommand::WriteVideo(data, codec)) => {
                        let _ = pub_cmd_tx.send(PcCommand::WriteVideo(data, codec)).await;
                    }
                    Some(TransportCommand::Shutdown) => {
                        let _ = pub_cmd_tx.send(PcCommand::Shutdown).await;
                        break;
                    }
                    None => break,
                }
            }
            // A `None` here means that I/O loop has ended, which only happens when the
            // transport is being torn down. Breaking rather than ignoring it matters:
            // `recv` on a closed channel returns immediately and forever, so an ignored
            // `None` turns this select into a busy loop that spins a core until the
            // process exits.
            ev = pub_event_rx.recv() => {
                let Some(ev) = ev else { break };
                let _ = event_tx.send(TransportEvent::PublisherEvent(ev)).await;
            }
            ev = sub_event_rx.recv() => {
                let Some(ev) = ev else { break };
                let _ = event_tx.send(TransportEvent::SubscriberEvent(ev)).await;
            }
        }
    }
    tracing::info!("Transport dispatch ended");
}
