use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use elementium_e2ee::E2eeContext;
use elementium_media::audio_playback::AudioSink;
use elementium_types::{MediaTrackKey, PlaintextMedia, VideoFrame, WireMedia};

use crate::e2ee_io::{EncryptionPolicy, StartupHold, encrypt_or_drop, maybe_decrypt_event};
use crate::peer_connection::{
    self, PcEvent, PeerConnectionHandle, discover_and_add_srflx, lock_pc,
};

/// Bind a UDP socket at `addr` and look up its local address, tagging any failure with
/// which `role` the socket was being set up for.
///
/// Shared by [`WebRtcEngine::create_connection`] and
/// [`crate::livekit::transport::Transport::new_with_e2ee`] -- see
/// [`crate::error::SocketRole`] for why the sharing is safe under constitution principle
/// I. `addr` is a parameter (production callers always pass `"0.0.0.0:0"`) so a test can
/// force a genuine bind failure by colliding two sockets on the same fixed address,
/// rather than asserting on a hand-constructed error value.
pub(crate) fn bind_socket(
    addr: &str,
    role: crate::error::SocketRole,
) -> Result<(UdpSocket, std::net::SocketAddr), crate::error::SocketSetupError> {
    let socket = UdpSocket::bind(addr).map_err(|source| crate::error::SocketSetupError {
        role,
        step: crate::error::SocketSetupStep::Bind,
        source,
    })?;
    let local_addr = socket
        .local_addr()
        .map_err(|source| crate::error::SocketSetupError {
            role,
            step: crate::error::SocketSetupStep::LocalAddr,
            source,
        })?;
    Ok((socket, local_addr))
}

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
    ///
    /// The [`MediaTrackKey`] says which of our tracks the frame belongs to. Audio has two
    /// once a share is running -- the microphone and the shared application -- and they go
    /// to different m-lines.
    WriteAudio(MediaTrackKey, PlaintextMedia),
    /// Write an encoded video frame to the peer connection. See [`IoCommand::WriteAudio`].
    ///
    /// The codec travels with the frame rather than being assumed. Two things downstream
    /// need it and get it wrong in different ways: the payload type written into the RTP
    /// header, and how much of the frame E2EE leaves in the clear. The second is invisible
    /// to the sender -- a frame framed by the wrong codec's rules is one only the peer
    /// notices, by being unable to authenticate it.
    ///
    /// The track key is here for the same class of reason: a camera frame and a screen
    /// frame are both video, and sending one down the other's m-line fails in a way only
    /// the receivers can see.
    WriteVideo(
        MediaTrackKey,
        PlaintextMedia,
        elementium_codec::VideoCodec,
    ),
    /// Shut down the I/O loop.
    Shutdown,
}

/// A shared buffer of the latest decoded video frame per track.
/// The protocol handler reads from this to serve frames to the webview.
pub type VideoFrameBuffer = Arc<Mutex<HashMap<String, VideoFrame>>>;

/// The receiving ends of a connection's two event channels, handed to whichever task
/// forwards them.
///
/// Owned rather than shared behind a lock. The forwarder is the sole consumer of both, and
/// owning them is what lets it `await` a message instead of polling one out from under a
/// mutex -- see [`ManagedPc::take_receivers`].
pub struct PcReceivers {
    /// High-rate media events (`AudioData`/`VideoData`), bounded so a stalled consumer
    /// sheds load instead of growing without limit. See [`PcEvent::is_media`].
    pub event_rx: mpsc::Receiver<PcEvent>,
    /// Control events (connection/ICE state, candidates, stats, keyframe requests).
    /// Unbounded: each is the only notice of a transition that will not repeat, so this
    /// side of the split must never drop under backpressure. Defensible as unbounded
    /// specifically because these are rare and bounded by real state transitions, not by
    /// anything an adversary or a busy network could drive unboundedly -- there is no
    /// event source here that fires faster than the state machine it reports on.
    pub control_rx: mpsc::UnboundedReceiver<PcEvent>,
}

/// Info about a managed peer connection.
pub struct ManagedPc {
    pub handle: PeerConnectionHandle,
    pub socket: Arc<UdpSocket>,
    pub io_cmd_tx: mpsc::Sender<IoCommand>,
    /// The event receivers, until something takes them. See [`ManagedPc::take_receivers`].
    receivers: Mutex<Option<PcReceivers>>,
    /// The `peer_connection` tracing span this connection was created under, carrying
    /// its `correlation_id`. Retained so later operations on this connection (e.g.
    /// closing it) can re-enter the same span.
    pub span: tracing::Span,
}

impl ManagedPc {
    /// Take ownership of this connection's event receivers, leaving nothing behind.
    ///
    /// Returns `None` on the second call: exactly one task may consume these channels, and
    /// a second consumer would silently split the event stream between them rather than
    /// failing. Making that unrepresentable is the point of moving the receivers out
    /// instead of sharing them behind a mutex -- the previous shape let any number of
    /// pollers race for each message, and forced the one real consumer to take a lock per
    /// event on the hot path.
    #[must_use]
    pub fn take_receivers(&self) -> Option<PcReceivers> {
        self.receivers.lock().ok()?.take()
    }
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
        let (socket, local_addr) = bind_socket("0.0.0.0:0", crate::error::SocketRole::Connection)?;

        // Add the socket address as a local ICE candidate (host)
        peer_connection::add_local_candidate(&mut pc_inner, local_addr);

        // Perform STUN discovery using provided ICE servers
        if let Some(servers) = ice_servers {
            discover_and_add_srflx(&socket, &mut pc_inner, local_addr, servers);
        }

        let handle: PeerConnectionHandle = Arc::new(Mutex::new(pc_inner));
        let socket = Arc::new(socket);

        // Channels for the I/O loop. Media (`event_tx`) stays bounded and droppable;
        // control (`control_tx`) is unbounded so a burst of media never costs it an event
        // -- see the fields' doc comments on `ManagedPc` and `PcEvent::is_media`.
        let (io_cmd_tx, io_cmd_rx) = mpsc::channel::<IoCommand>(256);
        let (event_tx, event_rx) = mpsc::channel::<PcEvent>(256);
        let (control_tx, control_rx) = mpsc::unbounded_channel::<PcEvent>();

        // Spawn the I/O loop as a blocking task (it does synchronous UDP I/O)
        let loop_handle = handle.clone();
        let loop_socket = socket.clone();
        let loop_e2ee = self.e2ee.clone(); // clones the Arc, shares the policy
        let loop_span = span.clone();
        tokio::task::spawn_blocking(move || {
            let _enter = loop_span.enter();
            io_loop(
                &loop_handle,
                &loop_socket,
                io_cmd_rx,
                &event_tx,
                &control_tx,
                &loop_e2ee,
            );
        });

        self.connections.insert(
            id,
            ManagedPc {
                handle,
                socket,
                io_cmd_tx,
                receivers: Mutex::new(Some(PcReceivers {
                    event_rx,
                    control_rx,
                })),
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

    /// How many connections the engine holds.
    ///
    /// Separate from [`WebRtcEngine::connection_ids`] because the count has a caller that runs
    /// on a timer -- the forwarder's leak census, which compares it against how many event
    /// forwarders are alive -- and cloning every id to then call `.len()` on the vector is a
    /// waste in exactly the situation the census exists to notice, where there are far too
    /// many of them.
    #[must_use]
    pub fn connection_count(&self) -> usize {
        self.connections.len()
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
    /// Writes the pacer below held back for at least one iteration.
    spaced: u64,
    /// Times the pacer found the backlog too deep to keep pacing and flushed instead.
    gave_up: u64,
    /// An audio write already pulled off the command channel and encrypted, held here
    /// because `decide_audio_pace` said it was early. Only ever one at a time: as long as
    /// this is `Some`, `drain_io_commands` will not pull another audio write out of order
    /// behind it, so a second slot is never needed.
    deferred: Option<(MediaTrackKey, Vec<WireMedia>)>,
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
                spaced = self.spaced,
                gave_up = self.gave_up,
                "Outbound audio wire pacing"
            );
            self.burst_writes = 0;
            self.max_gap_ms = 0;
            self.spaced = 0;
            self.gave_up = 0;
        }
    }
}

/// What `decide_audio_pace` chose to do with one queued audio write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioPaceDecision {
    /// Write now: no backlog, pacing disabled, an already-late frame, or a healthy gap.
    /// This is the only outcome a single-frame stream (rule 1) ever sees.
    Send,
    /// Hold for one more I/O loop iteration: this write would otherwise land early, and the
    /// backlog behind it is still shallow enough to be worth catching up on.
    Defer,
    /// Backlog is deep enough that continuing to hold writes back would grow the queue
    /// faster than the burst it is meant to prevent; write now instead.
    GiveUp,
}

/// Below this gap, two consecutive sends are "clumped" -- matches
/// `AudioSendPacing::CLUMPED_BELOW_US` in `peer_connection.rs`, the instrumentation that first
/// measured this. A write processed sooner than this after the last one is only early
/// because it queued up behind something, not because it is actually due, so only writes
/// this close to the last one are worth spacing out.
/// How often to report media dropped because the forwarder is behind.
///
/// The first is always reported, so the condition is never invisible, and then one in this
/// many. Reporting every one cost more than the drop did.
const DROPPED_EVENT_REPORT_EVERY: u64 = 500;

const AUDIO_PACE_EARLY_BELOW: Duration = Duration::from_millis(5);

/// Nominal Opus frame cadence, and the ceiling a deferred write may be pushed out to. A write
/// already at least this far behind the last one has already missed its slot -- holding it
/// longer only adds mouth-to-ear latency without curing whatever caused the stall (rule 2).
const AUDIO_PACE_NOMINAL_CADENCE: Duration = Duration::from_millis(20);

/// How many writes may sit queued behind the one being paced before the pacer gives up and
/// flushes instead of deferring further (rule 3). Four is one nominal cadence period's worth
/// of catch-up (4 x 20ms = 80ms of backlog); past that, continuing to hold audio back grows
/// the queue faster than letting the burst through would, which defeats the point of pacing.
const AUDIO_PACE_MAX_BACKLOG: usize = 4;

/// Pure pacing policy for one audio write.
///
/// Given whether pacing is enabled at all, how long it has been since the previous write
/// actually reached str0m, how many more audio writes are already queued behind this one,
/// and whether this write is already running at or past the nominal cadence, decide whether
/// to send it now, hold it for one more I/O loop iteration, or give up pacing and flush.
///
/// Pure and free of the clock/channel/socket around it, so the policy itself -- not the
/// plumbing -- is what the unit tests below pin.
fn decide_audio_pace(
    pacing_enabled: bool,
    since_last_write: Duration,
    backlog_depth: usize,
    already_late: bool,
) -> AudioPaceDecision {
    if !pacing_enabled {
        return AudioPaceDecision::Send;
    }
    // Rule 1: a lone frame with nothing queued behind it takes the untouched path.
    if backlog_depth == 0 {
        return AudioPaceDecision::Send;
    }
    // Rule 2: never delay a frame that is already behind, or merely on schedule -- only a
    // frame that would land early is worth spacing out.
    if already_late || since_last_write >= AUDIO_PACE_EARLY_BELOW {
        return AudioPaceDecision::Send;
    }
    // Rule 3: catching up is bounded -- a backlog this deep is worse held than flushed.
    if backlog_depth > AUDIO_PACE_MAX_BACKLOG {
        return AudioPaceDecision::GiveUp;
    }
    AudioPaceDecision::Defer
}

/// Whether the audio write pacer (`decide_audio_pace`) is active at all.
///
/// `ELEMENTIUM_AUDIO_PACING=0` disables it, restoring today's send-immediately behaviour.
/// This exists because the burst the pacer targets was only ever measured on a real 2-minute
/// call (see `AudioSendPacing`'s docs in `peer_connection.rs`) -- there is no offline repro to
/// validate against, so the next real call needs an easy way to A/B this change.
///
/// Read once via `OnceLock`, the same pattern as `max_encode_fps()` in
/// `src-tauri/src/commands/media_devices.rs`.
fn audio_pacing_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        let disabled = std::env::var_os("ELEMENTIUM_AUDIO_PACING")
            .is_some_and(|v| v == std::ffi::OsStr::new("0"));
        if disabled {
            tracing::info!("audio write pacing disabled via ELEMENTIUM_AUDIO_PACING=0");
        }
        !disabled
    })
}

/// Send `frames` now and record pacing stats. Shared by the direct path and by a write
/// released from the pacer's one-slot hold.
fn write_audio_now(
    handle: &PeerConnectionHandle,
    key: MediaTrackKey,
    frames: &[WireMedia],
    pacing: &mut WritePacing,
) {
    let mut pc = lock_pc(handle);
    pacing.record(&pc.id.clone());
    let mut failure = None;
    for data in frames {
        if let Err(e) = peer_connection::write_audio(&mut pc, key, data) {
            failure = Some(e);
        }
    }
    if let Some(e) = failure {
        // Throttled warn, not debug: this is the last hop before the network, so a
        // persistent failure here means nobody hears us -- and at `debug` it produced a
        // completely clean log while doing so. The usual causes (no audio mid negotiated,
        // no Opus payload type) are steady-state, not transient, hence the coarse throttle.
        static WRITE_FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
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

/// Send `frames` now, or hold them in `pacing` for one more I/O loop iteration, per
/// `decide_audio_pace`.
///
/// `backlog_depth` counts everything still queued behind this write, audio and video alike
/// -- there is no cheap way to filter the channel to audio-only without pulling items out of
/// it. Video is far lower rate, so this only ever over-counts, which costs an unnecessary
/// defer, never a missed one.
fn dispatch_audio_write(
    handle: &PeerConnectionHandle,
    cmd_rx: &mpsc::Receiver<IoCommand>,
    pacing: &mut WritePacing,
    key: MediaTrackKey,
    frames: Vec<WireMedia>,
) {
    let now = Instant::now();
    let since_last_write = pacing
        .last_write
        .map_or(Duration::MAX, |last| now.saturating_duration_since(last));
    let already_late = since_last_write >= AUDIO_PACE_NOMINAL_CADENCE;
    let backlog_depth = cmd_rx.len();

    match decide_audio_pace(
        audio_pacing_enabled(),
        since_last_write,
        backlog_depth,
        already_late,
    ) {
        AudioPaceDecision::Defer => {
            pacing.spaced = pacing.spaced.saturating_add(1);
            pacing.deferred = Some((key, frames));
        }
        AudioPaceDecision::GiveUp => {
            pacing.gave_up = pacing.gave_up.saturating_add(1);
            write_audio_now(handle, key, &frames, pacing);
        }
        AudioPaceDecision::Send => write_audio_now(handle, key, &frames, pacing),
    }
}

/// Encrypt one queued audio write and hand it to the pacer.
fn handle_write_audio(
    handle: &PeerConnectionHandle,
    cmd_rx: &mpsc::Receiver<IoCommand>,
    e2ee: Option<&E2eeContext>,
    hold: &mut StartupHold,
    pacing: &mut WritePacing,
    key: MediaTrackKey,
    opus_data: PlaintextMedia,
) {
    // Usually one frame. More when end-to-end encryption has just become possible and the
    // audio captured while it came up is released together.
    let frames = hold.encrypt_audio(e2ee, opus_data);
    if frames.is_empty() {
        return;
    }
    dispatch_audio_write(handle, cmd_rx, pacing, key, frames);
}

/// Drain any pending I/O commands (non-blocking). Returns `true` if the
/// I/O loop should shut down.
fn drain_io_commands(
    cmd_rx: &mut mpsc::Receiver<IoCommand>,
    handle: &PeerConnectionHandle,
    e2ee: Option<&E2eeContext>,
    pacing: &mut WritePacing,
    hold: &mut StartupHold,
) -> bool {
    // Flush anything the pacer held back last iteration before pulling anything new off the
    // channel, so audio can never leave out of order behind it.
    if let Some((key, frames)) = pacing.deferred.take() {
        dispatch_audio_write(handle, cmd_rx, pacing, key, frames);
        if pacing.deferred.is_some() {
            // Still early (or the backlog grew): wait for the next iteration rather than
            // drain more audio out of order behind it.
            return false;
        }
    }

    loop {
        match cmd_rx.try_recv() {
            Ok(IoCommand::WriteAudio(key, opus_data)) => {
                handle_write_audio(handle, cmd_rx, e2ee, hold, pacing, key, opus_data);
                if pacing.deferred.is_some() {
                    // Just queued this write for the pacer to hold; stop draining so
                    // nothing behind it can be sent first.
                    return false;
                }
            }
            Ok(IoCommand::WriteVideo(key, frame, codec)) => {
                let Some(kind) = crate::e2ee_io::video_media_kind(codec) else {
                    tracing::warn!(
                        reason = "no_e2ee_framing_for_codec",
                        codec = codec.sdp_name(),
                        "Dropping outbound video frame: this codec has no E2EE framing"
                    );
                    continue;
                };
                let Some(data) = encrypt_or_drop(e2ee, frame, kind, "video") else {
                    continue;
                };
                let mut pc = lock_pc(handle);
                if let Err(e) = peer_connection::write_video(&mut pc, key, &data, codec) {
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
    control_tx: &mpsc::UnboundedSender<PcEvent>,
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
    let mut startup_hold = StartupHold::default();

    loop {
        // Snapshot the E2EE policy for this iteration.
        // EncryptionPolicy::clone() is cheap (Arc::clone inside E2eeContext), and this
        // picks up contexts that were initialized after the I/O loop started.
        let e2ee: EncryptionPolicy = e2ee_ctx.lock().map(|g| g.clone()).unwrap_or_default();

        // Process any pending commands (non-blocking)
        if drain_io_commands(
            &mut cmd_rx,
            handle,
            e2ee.as_context(),
            &mut write_pacing,
            &mut startup_hold,
        ) {
            return;
        }

        // Poll str0m for outputs, decrypt inbound if E2EE is active
        let deadline = {
            let mut pc = lock_pc(handle);
            match peer_connection::poll_once(&mut pc, socket, &mut recv_buf) {
                Ok((events, deadline)) => {
                    for event in events {
                        let Some(event) = maybe_decrypt_event(event, e2ee.as_context()) else {
                            continue;
                        };
                        // Media is droppable under backpressure; control is not. Splitting
                        // here rather than merging the two channels downstream is what
                        // keeps a media burst from ever being able to starve a control
                        // event of a slot -- there is no shared capacity to contend for.
                        if event.is_media() {
                            if event_tx.try_send(event).is_err() {
                                dropped_events = dropped_events.saturating_add(1);
                                // Throttled, having been written unthrottled on the
                                // assumption that a capacity of 256 meant it could not fire
                                // under normal load. It fires hundreds of times a second on
                                // an ordinary call with inbound video: the forwarder cannot
                                // drain media as fast as a remote participant produces it.
                                //
                                // The volume was not merely noise. Each occurrence formatted
                                // and wrote a line from inside the I/O loop, slowing the
                                // loop that was already behind, which dropped more events.
                                // A log line that makes its own subject worse is the exact
                                // shape the constitution's "throttle, do not flood" exists
                                // to prevent, and it went in as part of writing that rule.
                                if dropped_events == 1
                                    || dropped_events.is_multiple_of(DROPPED_EVENT_REPORT_EVERY)
                                {
                                    tracing::warn!(
                                        pc_id = %pc.id,
                                        dropped_events,
                                        "inbound media dropped: the forwarder is not keeping up"
                                    );
                                }
                            }
                        } else if control_tx.send(event).is_err() {
                            // Only fails when `forward_events` has already returned (the
                            // receiver dropped), which only happens after this connection
                            // was removed -- nothing downstream is listening to lose this
                            // event *to*, so it is not a loss in the sense this split
                            // exists to prevent.
                            tracing::debug!(
                                pc_id = %pc.id,
                                "control event undelivered: forward_events already exited"
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

#[cfg(test)]
mod audio_pace_tests {
    use super::{
        AUDIO_PACE_EARLY_BELOW, AUDIO_PACE_MAX_BACKLOG, AudioPaceDecision, decide_audio_pace,
    };
    use std::time::Duration;

    /// Rule 1: a healthy stream, one frame at a time, must take exactly the path it takes
    /// today -- zero backlog is `Send` regardless of how close the gap is, because there is
    /// nothing to pace against. This is the regression this whole design exists not to
    /// cause.
    #[test]
    fn single_frame_in_healthy_stream_is_never_deferred() {
        let decision = decide_audio_pace(true, Duration::from_millis(1), 0, false);
        assert_eq!(decision, AudioPaceDecision::Send);
    }

    /// A frame that would land 1ms after the previous one, with something queued behind it,
    /// is exactly the clumping this pacer exists to smooth out -- `Defer`.
    #[test]
    fn early_frame_with_backlog_is_deferred() {
        let decision = decide_audio_pace(true, Duration::from_millis(1), 1, false);
        assert_eq!(decision, AudioPaceDecision::Defer);
    }

    /// Sanity check on the boundary: a gap right at `AUDIO_PACE_EARLY_BELOW` is not "early"
    /// (the report's own `CLUMPED_BELOW_US` cutoff is exclusive), so this must send.
    #[test]
    fn gap_at_the_early_threshold_is_not_deferred() {
        let decision = decide_audio_pace(true, AUDIO_PACE_EARLY_BELOW, 1, false);
        assert_eq!(decision, AudioPaceDecision::Send);
    }

    /// Rule 2: a frame already flagged late must never be held, no matter how deep the
    /// backlog is -- holding it only adds mouth-to-ear latency without curing the stall that
    /// made it late.
    #[test]
    fn already_late_frame_is_never_deferred_regardless_of_backlog() {
        for backlog_depth in [1, AUDIO_PACE_MAX_BACKLOG, AUDIO_PACE_MAX_BACKLOG + 10] {
            let decision =
                decide_audio_pace(true, Duration::from_millis(1), backlog_depth, true);
            assert_eq!(
                decision,
                AudioPaceDecision::Send,
                "backlog_depth={backlog_depth} must still send a late frame"
            );
        }
    }

    /// Rule 3: catching up is bounded. Once the backlog is deeper than
    /// `AUDIO_PACE_MAX_BACKLOG`, the pacer must give up and flush rather than defer further,
    /// because holding a growing backlog is worse than letting the burst through.
    #[test]
    fn deep_backlog_gives_up_instead_of_deferring() {
        let decision = decide_audio_pace(
            true,
            Duration::from_millis(1),
            AUDIO_PACE_MAX_BACKLOG + 1,
            false,
        );
        assert_eq!(decision, AudioPaceDecision::GiveUp);
    }

    /// Rule 4: `ELEMENTIUM_AUDIO_PACING=0` disables the pacer entirely. Same inputs that
    /// would otherwise `Defer` must instead `Send`, as if there were no pacer at all.
    #[test]
    fn disabled_pacing_always_sends() {
        let decision = decide_audio_pace(false, Duration::from_millis(1), 1, false);
        assert_eq!(decision, AudioPaceDecision::Send);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod socket_setup_tests {
    use super::bind_socket;
    use crate::error::{SocketRole, SocketSetupError, SocketSetupStep};

    /// A genuine bind failure (two sockets fighting over the same fixed address, rather
    /// than the ephemeral `0.0.0.0:0` every real caller uses) must surface as
    /// `SocketSetupError::Bind` naming the role that was being set up, with the real
    /// `io::Error` still attached -- not a formatted string a caller could not match on.
    #[test]
    fn colliding_on_a_fixed_port_is_reported_as_a_bind_failure() {
        let holder = std::net::UdpSocket::bind("127.0.0.1:0").expect("first bind must succeed");
        let addr = holder.local_addr().expect("bound socket has a local addr");

        let result = bind_socket(&addr.to_string(), SocketRole::Publisher);
        assert!(
            matches!(
                result,
                Err(SocketSetupError {
                    role: SocketRole::Publisher,
                    step: SocketSetupStep::Bind,
                    ..
                })
            ),
            "{result:?}"
        );
    }

    /// A clean bind (the common case) must hand back a working socket and its real local
    /// address, not just avoid erroring.
    #[test]
    fn a_clean_bind_reports_its_own_local_address() {
        let (socket, addr) =
            bind_socket("127.0.0.1:0", SocketRole::Connection).expect("ephemeral bind must succeed");
        assert_eq!(socket.local_addr().expect("bound socket has a local addr"), addr);
    }

    /// `role` and `step` must both reach the message, not just one of them -- a reader
    /// diagnosing a `Transport` with three sockets needs to know which failed and how.
    #[test]
    fn the_role_and_step_both_reach_the_message() {
        let err = SocketSetupError {
            role: SocketRole::Subscriber,
            step: SocketSetupStep::LocalAddr,
            source: std::io::Error::other("simulated"),
        };
        let text = err.to_string();
        assert!(text.contains("subscriber"), "{text}");
        assert!(text.contains("local address lookup"), "{text}");
    }
}
