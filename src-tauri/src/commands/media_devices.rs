// Every `#[tauri::command]` async fn below that takes a `State<'_, T>` parameter causes
// the `#[command]` macro to generate a sibling IPC-dispatch wrapper item in this module
// containing an internal match with an arm clippy flags as unreachable. That wrapper is
// framework codegen (not nested inside the fn item itself, so a function- or
// statement-scoped `#[allow]` cannot reach it — verified empirically), hence the
// module-level allow here rather than the usual per-item scoping.
#![allow(clippy::unreachable)]
use std::sync::{Arc, Mutex};

use tauri::{State, command};
use tokio::sync::mpsc as tokio_mpsc;

use elementium_codec::{OpusEncoder, OpusEncoderConfig, Vp8Encoder};
use elementium_media::audio_capture::AudioCapturer;
use elementium_media::device_enumeration;
use elementium_types::observability::CorrelationId;
use elementium_types::{
    AudioFrame, MediaConstraints, MediaDevice, NetworkLossEstimate, TrackId, VideoFrame,
};
use elementium_webrtc::engine::{IoCommand, VideoFrameBuffer};

use super::LockExt;
use super::webrtc::WebRtcState;
use crate::protocols::VideoFrameState;

/// Handle to a running camera pipeline.
pub struct CameraPipelineHandle {
    pub track_id: String,
    pub stop_tx: std::sync::mpsc::Sender<()>,
    /// Set to enable VP8 encoding and sending to a peer connection.
    /// When `None`, the pipeline only writes RGBA frames for preview.
    pub encode_tx: Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
}

/// Handle to a running audio capture pipeline.
pub struct AudioCaptureHandle {
    pub track_id: String,
    pub stop_tx: std::sync::mpsc::Sender<()>,
    /// Set to enable Opus encoding and sending to a peer connection.
    /// When `None`, the pipeline captures but doesn't encode/send.
    pub encode_tx: Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
    /// Measured outbound packet loss, fed from RTCP receiver reports.
    ///
    /// Written by whoever observes `PcEvent::EgressStats`, read by the capture thread to
    /// size the Opus encoder's FEC redundancy. Shared rather than passed through the
    /// command channel because it is a continuously-updated level, not an event: the
    /// capture thread only cares about the latest value.
    pub loss_estimate: Arc<NetworkLossEstimate>,
}

/// State for active media tracks (audio capture, video capture, etc.).
pub struct MediaState {
    pub active_tracks: Mutex<Vec<TrackId>>,
    /// Active camera pipeline (at most one camera at a time).
    pub camera: Mutex<Option<CameraPipelineHandle>>,
    /// Active audio capture pipeline (at most one mic at a time).
    pub audio_capture: Mutex<Option<AudioCaptureHandle>>,
    /// Where captured media goes for the currently-connected SFU room, if any.
    ///
    /// Publishing a track attaches whatever pipeline is running at that moment, but the
    /// two events have no fixed order: joining a call muted and unmuting later starts the
    /// microphone long after its track was published, and that pipeline has no earlier
    /// handle to inherit a connection from. Remembering the room's sender here means a
    /// pipeline that starts at any point in a call is attached on startup.
    pub sfu_media_tx: Mutex<Option<tokio_mpsc::Sender<IoCommand>>>,
}

#[command]
pub async fn enumerate_devices() -> Result<Vec<MediaDevice>, String> {
    tracing::info!("Enumerating media devices");

    let mut devices = device_enumeration::enumerate_audio_devices();

    // Add video input devices
    // nokhwa device enumeration is best-effort
    if let Ok(cameras) = nokhwa::query(nokhwa::utils::ApiBackend::Auto) {
        for (i, cam) in cameras.iter().enumerate() {
            devices.push(MediaDevice {
                id: format!("video-input-{i}"),
                label: cam.human_name().clone(),
                kind: elementium_types::MediaDeviceKind::VideoInput,
            });
        }
    }

    Ok(devices)
}

/// A capture pipeline that can be stopped and whose peer connection can be handed on.
///
/// Implemented by both the audio and camera handles so the restart logic below is written
/// once. They had this bug independently and identically; sharing the code is what stops
/// them acquiring it again independently.
trait CapturePipeline {
    fn stop_sender(&self) -> &std::sync::mpsc::Sender<()>;
    fn connection(&self) -> &Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>;
}

impl CapturePipeline for AudioCaptureHandle {
    fn stop_sender(&self) -> &std::sync::mpsc::Sender<()> {
        &self.stop_tx
    }
    fn connection(&self) -> &Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>> {
        &self.encode_tx
    }
}

impl CapturePipeline for CameraPipelineHandle {
    fn stop_sender(&self) -> &std::sync::mpsc::Sender<()> {
        &self.stop_tx
    }
    fn connection(&self) -> &Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>> {
        &self.encode_tx
    }
}

/// What was found when replacing a capture pipeline.
struct StoppedPipeline {
    /// Whether a pipeline was actually running. Distinct from `connection.is_some()`: a
    /// pipeline can exist while never having been attached to a peer connection, and the
    /// camera needs this specifically to know whether to wait for the device to release.
    existed: bool,
    /// The peer connection the old pipeline was feeding, to hand to its replacement.
    connection: Option<tokio_mpsc::Sender<IoCommand>>,
}

/// The connection a freshly-started pipeline should feed.
///
/// Prefers whatever the pipeline it replaces was feeding, falling back to the connected
/// SFU room. Both are needed: the inherited connection covers a restart mid-call on the
/// direct-`WebRTC` path, and the room covers a pipeline that starts for the first time
/// after its track was already published (joining muted, then unmuting).
fn connection_for_new_pipeline(
    inherited: Option<tokio_mpsc::Sender<IoCommand>>,
    media_state: &MediaState,
) -> Option<tokio_mpsc::Sender<IoCommand>> {
    inherited.or_else(|| {
        media_state
            .sfu_media_tx
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    })
}

/// Start (or restart) the microphone pipeline, returning the new track's id.
///
/// Replaces whatever was running and takes over its connection, so a mid-call restart
/// keeps sending -- see [`stop_pipeline_inheriting_connection`].
fn start_audio_pipeline(media_state: &MediaState) -> TrackId {
    let track_id = TrackId(format!("audio-{}", generate_track_id()));
    tracing::info!(track_id = %track_id, "Starting audio capture");

    let previous = stop_pipeline_inheriting_connection(&media_state.audio_capture);
    let connection = connection_for_new_pipeline(previous.connection, media_state);
    if connection.is_some() {
        tracing::info!(
            track_id = %track_id,
            "Audio capture attached to a live call on startup"
        );
    }

    let encode_tx: Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>> =
        Arc::new(Mutex::new(connection));
    let encode_tx_clone = encode_tx.clone();
    // Seeded with the encoder's own starting assumption so behaviour before the
    // first RTCP report matches the configured default, then converges on measured
    // loss as reports arrive.
    let loss_estimate = Arc::new(NetworkLossEstimate::new(
        OpusEncoderConfig::default().expected_packet_loss_perc,
    ));
    let loss_estimate_clone = loss_estimate.clone();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();

    // Inherits the call's correlation span so every event the thread emits carries the
    // same correlation_id.
    let audio_span = tracing::Span::current();
    std::thread::spawn(move || {
        let _guard = audio_span.enter();
        audio_capture_loop(&encode_tx_clone, &stop_rx, &loss_estimate_clone);
    });

    if let Ok(mut audio) = media_state.audio_capture.lock() {
        *audio = Some(AudioCaptureHandle {
            track_id: track_id.0.clone(),
            stop_tx,
            encode_tx,
            loss_estimate,
        });
    }

    if let Ok(mut tracks) = media_state.active_tracks.lock() {
        tracks.push(track_id.clone());
    }

    track_id
}

/// Stop the running capture pipeline, returning the peer connection it was feeding.
///
/// Carrying the connection over to the replacement is load-bearing, not tidiness.
/// `getUserMedia` is re-called during a live call (mute/unmute, device change, track
/// replacement -- five times in one short session in a real log), and a replacement that
/// started disconnected stayed disconnected until the next renegotiation happened to
/// occur, which may be never: the log shows an audio restart after which
/// `skipped_not_connected` simply climbed forever while `sent_frames` stayed at zero. The
/// far end hears the speaker cut out, or the camera freeze, mid-call for no visible
/// reason.
fn stop_pipeline_inheriting_connection<H: CapturePipeline>(
    slot: &Mutex<Option<H>>,
) -> StoppedPipeline {
    let Ok(mut guard) = slot.lock() else {
        return StoppedPipeline { existed: false, connection: None };
    };
    let Some(old) = guard.take() else {
        return StoppedPipeline { existed: false, connection: None };
    };
    let _ = old.stop_sender().send(());
    let connection = old.connection().lock().ok().and_then(|c| c.clone());
    StoppedPipeline { existed: true, connection }
}

#[command]
pub async fn get_user_media(
    webrtc_state: State<'_, WebRtcState>,
    media_state: State<'_, MediaState>,
    constraints: MediaConstraints,
) -> Result<Vec<TrackId>, String> {
    let call_id = CorrelationId::new();
    let call_span = tracing::info_span!(
        "call",
        correlation_id = %call_id,
        audio_requested = constraints.audio.is_some(),
        video_requested = constraints.video.is_some(),
    );
    let _call_guard = call_span.enter();

    tracing::info!(?constraints, "getUserMedia request");
    let mut track_ids = Vec::new();

    if constraints.audio.is_some() {
        track_ids.push(start_audio_pipeline(&media_state));
    }

    if let Some(ref video_constraints) = constraints.video {
        let track_id = TrackId(format!("video-{}", generate_track_id()));
        tracing::info!(track_id = %track_id, "Starting video capture");

        // Get the shared video frame buffer from the WebRTC engine
        let video_frames = {
            let engine = webrtc_state.0.lock_str()?;
            engine.video_frames.clone()
        };

        // Stop any existing camera pipeline, inheriting whichever peer connection it was
        // feeding, and note whether we need to wait for the device to release.
        //
        // The inheritance matters for exactly the reason it does on the audio path: a
        // camera restarted mid-call (device change, resolution change, track replacement)
        // would otherwise sit disconnected until the next renegotiation, and the far end
        // would see the video freeze with nothing in the log to explain it.
        let previous = stop_pipeline_inheriting_connection(&media_state.camera);
        let had_previous = previous.existed;
        let inherited_connection = connection_for_new_pipeline(previous.connection, &media_state);
        if inherited_connection.is_some() {
            tracing::info!(
                track_id = %track_id,
                "Camera restarted mid-call; inheriting the existing peer connection"
            );
        }

        let req_width = video_constraints.width;
        let req_height = video_constraints.height;

        let encode_tx: Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>> =
            Arc::new(Mutex::new(inherited_connection));
        let encode_tx_clone = encode_tx.clone();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let tid = track_id.0.clone();

        // Start the camera pipeline on a background thread, inheriting the
        // call's correlation span so every event it emits carries the same
        // correlation_id.
        // If we just stopped a previous pipeline, delay to let the V4L2
        // device release (avoids EBUSY on Linux).
        let camera_span = tracing::Span::current();
        std::thread::spawn(move || {
            let _guard = camera_span.enter();
            if had_previous {
                tracing::info!("Waiting for previous camera to release device...");
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            camera_pipeline_loop(
                &tid,
                &video_frames,
                &encode_tx_clone,
                &stop_rx,
                req_width,
                req_height,
            );
        });

        // Store the camera pipeline handle
        if let Ok(mut cam) = media_state.camera.lock() {
            *cam = Some(CameraPipelineHandle {
                track_id: track_id.0.clone(),
                stop_tx,
                encode_tx,
            });
        }

        if let Ok(mut tracks) = media_state.active_tracks.lock() {
            tracks.push(track_id.clone());
        }
        track_ids.push(track_id);
    }

    Ok(track_ids)
}

#[command]
pub async fn stop_track(
    media_state: State<'_, MediaState>,
    track_id: TrackId,
) -> Result<(), String> {
    tracing::info!(%track_id, "Stopping track");

    // If this is the camera track, stop the pipeline
    if track_id.0.starts_with("video-")
        && let Ok(mut cam) = media_state.camera.lock()
        && let Some(ref handle) = *cam
        && handle.track_id == track_id.0
    {
        let _ = handle.stop_tx.send(());
        *cam = None;
    }

    // If this is an audio track, stop the audio capture pipeline
    if track_id.0.starts_with("audio-")
        && let Ok(mut audio) = media_state.audio_capture.lock()
        && let Some(ref handle) = *audio
        && handle.track_id == track_id.0
    {
        let _ = handle.stop_tx.send(());
        *audio = None;
    }

    if let Ok(mut tracks) = media_state.active_tracks.lock() {
        tracks.retain(|t| t != &track_id);
    }
    Ok(())
}

/// Fetch the latest video frame for a track as raw bytes via IPC.
///
/// Returns an 8-byte header (width: u32 LE, height: u32 LE) followed by RGBA data.
/// Returns an 8-byte zero header when no frame is available.
// `state` and `track_id` are only borrowed internally, but tauri's IPC command
// extractors require owned `State<'_, T>` and `String` parameters (the latter must be
// owned because it's deserialized from the IPC payload) — the signature can't be
// changed to take references without breaking command registration.
#[allow(clippy::needless_pass_by_value)]
#[command]
pub fn get_video_frame(
    state: State<'_, VideoFrameState>,
    track_id: String,
) -> tauri::ipc::Response {
    static CALL_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let frame = state.0.lock().ok().and_then(|f| f.get(&track_id).cloned());

    let count = CALL_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if count < 3 || count.is_multiple_of(300) {
        tracing::info!(
            track_id = %track_id,
            has_frame = frame.is_some(),
            count,
            "get_video_frame IPC call"
        );
    }

    match frame {
        Some(f) => {
            let mut body = Vec::with_capacity(f.data.len().saturating_add(8));
            body.extend_from_slice(&f.width.to_le_bytes());
            body.extend_from_slice(&f.height.to_le_bytes());
            body.extend_from_slice(&f.data);
            tauri::ipc::Response::new(body)
        }
        None => tauri::ipc::Response::new(vec![0u8; 8]),
    }
}

/// How long a newly-subscribed peer may wait before it can decode anything.
///
/// Short enough that joining a call feels immediate, long enough that the cost of
/// rebuilding the encoder's rate control is negligible.
const KEYFRAME_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// The fastest we encode, regardless of how fast the camera runs.
///
/// The webcam delivers 60fps. Encoding all of it doubles the bitrate needed for the same
/// picture quality and doubles the CPU cost, for a difference nobody watching a video call
/// can see. The preview still shows every captured frame; only the encoder skips.
const MAX_ENCODE_FPS: u64 = 30;

/// Minimum gap between encoded frames, from [`MAX_ENCODE_FPS`].
const MIN_ENCODE_INTERVAL: std::time::Duration =
    std::time::Duration::from_nanos(1_000_000_000 / MAX_ENCODE_FPS);

/// Pick a VP8 bitrate for a given frame size.
///
/// A fixed 500kbps was used for every resolution. At 1280x720 that is roughly a tenth of
/// what the picture needs: the encoder meets the budget by discarding detail, and what
/// arrives is blocky and smeared with colour bleeding across blocks. It looks like a
/// transmission fault and is not one.
///
/// The rate is ~0.1 bits per pixel per frame at [`MAX_ENCODE_FPS`], which is the usual
/// rule of thumb for VP8 at conversational quality, clamped to a sane range.
fn bitrate_for(width: u32, height: u32) -> u32 {
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    let bits_per_second = pixels.saturating_mul(MAX_ENCODE_FPS).saturating_div(10);
    let kbps = bits_per_second.saturating_div(1000);
    u32::try_from(kbps.clamp(300, 4000)).unwrap_or(2000)
}

/// Background thread: reads camera frames, writes RGBA to `VideoFrameBuffer` for
/// preview, and optionally VP8-encodes + sends to a peer connection.
fn camera_pipeline_loop(
    track_id: &str,
    video_frames: &VideoFrameBuffer,
    encode_tx: &Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
    stop_rx: &std::sync::mpsc::Receiver<()>,
    req_width: Option<u32>,
    req_height: Option<u32>,
) {
    // Prefers PipeWire, falls back to V4L2. On a desktop where PipeWire holds the camera,
    // V4L2 cannot work at all -- see `VideoSource`.
    let capturer = match elementium_media::video_source::VideoSource::start(req_width, req_height) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(reason = %e, track_id = %track_id, "Failed to start camera");
            return;
        }
    };

    let (width, height) = capturer.size();
    tracing::info!(
        width,
        height,
        backend = capturer.backend(),
        track_id = %track_id,
        "Camera pipeline started"
    );

    let mut encoder: Option<Vp8Encoder> = None;
    let mut frame_count: u64 = 0;
    let mut last_keyframe = std::time::Instant::now();
    let mut last_encode = std::time::Instant::now()
        .checked_sub(MIN_ENCODE_INTERVAL)
        .unwrap_or_else(std::time::Instant::now);

    loop {
        if stop_rx.try_recv().is_ok() {
            tracing::info!(track_id = %track_id, "Camera pipeline stopping");
            // Clean up the frame buffer entry
            if let Ok(mut buf) = video_frames.lock() {
                buf.remove(track_id);
            }
            break;
        }

        if let Some(frame) = capturer.try_recv() {
            frame_count = frame_count.wrapping_add(1);
            if frame_count <= 3 || frame_count.is_multiple_of(100) {
                tracing::info!(
                    track_id = %track_id,
                    frame_count,
                    w = frame.width,
                    h = frame.height,
                    data_len = frame.data.len(),
                    "Camera frame received"
                );
            }
            // Write RGBA frame to VideoFrameBuffer for local preview
            if let Ok(mut buf) = video_frames.lock() {
                buf.insert(
                    track_id.to_string(),
                    VideoFrame {
                        width: frame.width,
                        height: frame.height,
                        data: frame.data.clone(),
                        timestamp_us: 0,
                    },
                );
            }

            // VP8 encode and send if encoding is active
            let should_encode = encode_tx.lock().is_ok_and(|g| g.is_some());

            // The preview above gets every captured frame; the encoder is rate-limited.
            if should_encode && last_encode.elapsed() >= MIN_ENCODE_INTERVAL {
                last_encode = std::time::Instant::now();
                encode_and_send_video_frame(
                    track_id,
                    &frame,
                    &mut encoder,
                    &mut last_keyframe,
                    encode_tx,
                );
            }
        } else {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

/// VP8-encode one captured frame and hand the packets to the connection.
///
/// Owns the encoder's lifecycle because two things can invalidate it: the source
/// renegotiating its geometry, and the need for a periodic keyframe.
fn encode_and_send_video_frame(
    track_id: &str,
    frame: &elementium_types::VideoFrame,
    encoder: &mut Option<Vp8Encoder>,
    last_keyframe: &mut std::time::Instant,
    encode_tx: &Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
) {
    // Created lazily, and rebuilt if the source renegotiates its geometry:
    // `Vp8Encoder::encode` rejects a frame whose size does not match its configuration, so
    // a mid-stream resolution change would otherwise fail every frame from then on and
    // stop the video permanently.
    if encoder
        .as_ref()
        .is_none_or(|e| e.size() != (frame.width, frame.height))
    {
        let bitrate = bitrate_for(frame.width, frame.height);
        match Vp8Encoder::new(frame.width, frame.height, bitrate) {
            Ok(enc) => {
                tracing::info!(
                    width = frame.width,
                    height = frame.height,
                    bitrate_kbps = bitrate,
                    max_fps = MAX_ENCODE_FPS,
                    "VP8 encoder created for camera"
                );
                *encoder = Some(enc);
                *last_keyframe = std::time::Instant::now();
            }
            Err(e) => tracing::error!("Failed to create VP8 encoder: {e}"),
        }
    } else if last_keyframe.elapsed() >= KEYFRAME_INTERVAL
        && let Some(enc) = encoder.as_mut()
    {
        // Nothing routes a receiver's keyframe request back to this thread yet, so a peer
        // that subscribes mid-call would otherwise wait out libvpx's default keyframe
        // distance -- minutes -- before it could decode anything.
        match enc.force_keyframe() {
            Ok(()) => {
                *last_keyframe = std::time::Instant::now();
                tracing::debug!(track_id, "forced a VP8 keyframe");
            }
            Err(e) => tracing::warn!(track_id, reason = %e, "keyframe failed"),
        }
    }

    let Some(enc) = encoder.as_mut() else {
        return;
    };
    let i420 = elementium_codec::rgba_to_i420(frame.width, frame.height, &frame.data);
    match enc.encode(&i420) {
        Ok(packets) => {
            if let Ok(guard) = encode_tx.lock()
                && let Some(tx) = guard.as_ref()
            {
                for packet in packets {
                    let _ = tx.try_send(IoCommand::WriteVideo(packet.data));
                }
            }
        }
        Err(e) => tracing::debug!("VP8 encode error: {e}"),
    }
}

/// Counters for one run of [`audio_capture_loop`], so a silent far end is diagnosable.
#[derive(Default)]
struct OutboundAudioStats {
    captured: u64,
    encoded: u64,
    sent: u64,
    skipped_not_connected: u64,
    dropped_channel_full: u64,
    encode_errors: u64,
    /// Loudest raw sample seen since the last report; reset on each report so a quiet
    /// period is visible as a quiet period rather than being masked by an earlier peak.
    peak_since_report: f32,
    last_encoded_len: usize,
    /// Loss percentage currently configured on the encoder, so it is only reconfigured
    /// when the measured estimate actually moves.
    applied_packet_loss_perc: u8,
    /// Total encoded bytes this window, for a real transmitted bitrate.
    bytes_since_report: u64,
    /// Frames this window that encoded down to a silence-sized packet.
    silent_packets_since_report: u64,
    /// Frames this window that were audibly loud on input yet still encoded to a
    /// silence-sized packet.
    ///
    /// The direct test of a specific suspicion: reports repeatedly showed a 3-byte
    /// `last_encoded_len` while `input_peak_amplitude` was 0.4-0.6, i.e. loud speech. If
    /// this counter is anything but ~0, the encoder is discarding speech as silence and
    /// the far end is being sent nothing to reconstruct -- which sounds exactly like a
    /// robot. If it stays at 0, that pairing was just the window boundary landing in
    /// pauses, and the encoder is behaving.
    loud_but_silent_since_report: u64,
    /// When the previous frame was handed to the transport, for pacing measurement.
    last_frame_at: Option<std::time::Instant>,
    /// Largest gap between consecutive frames this window, in milliseconds.
    max_gap_ms: u64,
    /// Frames emitted back-to-back (< 5ms after the previous one), i.e. in a burst.
    ///
    /// str0m runs with `PacerImpl::null()` because we do not enable BWE, so it transmits
    /// each packet the instant we hand it over -- packets reach the wire exactly as
    /// bursty as this thread produces them. If cpal delivers large input buffers, several
    /// 20ms Opus frames are produced at once and then nothing for tens of milliseconds.
    /// The receiver sees clumps rather than a steady 50/sec stream, and a jitter buffer
    /// too small for the clump underruns and fills the gap with packet-loss concealment
    /// -- which sounds robotic even though not a single packet was lost, and is exactly
    /// consistent with RTCP reporting 0% loss.
    burst_frames: u64,
}

/// Channels the outbound stream is encoded as, regardless of what the capture device
/// offers.
///
/// Voice is mono everywhere in WebRTC. Encoding a microphone as stereo split the bitrate
/// across two near-identical channels, halving the bits spent on the only content that
/// matters -- audible as a thin, artifacty "robotic" quality that no pipeline counter can
/// detect, because the encoder is faithfully producing what it was asked for.
///
/// It also made the SDP a lie: RFC 7587 defaults `sprop-stereo` to 0 and nothing in the
/// offer/answer path ever set it, so a stereo stream was described to every receiver as
/// mono. Encoding mono makes the declaration true rather than papering over it.
const OUTBOUND_CHANNELS: u16 = 1;

/// Opus packets at or below this size carry no meaningful audio (DTX/comfort noise is
/// 1-3 bytes; a genuinely encoded frame is far larger).
const SILENT_PACKET_BYTES: usize = 5;

/// Input peak above which a frame is considered to contain real speech rather than room
/// noise. -40 dBFS: comfortably above a noise floor, well below normal speech.
const LOUD_INPUT_PEAK: f32 = 0.01;

impl OutboundAudioStats {
    /// How often to emit a summary: every 250 frames of 20ms, i.e. roughly every 5s.
    const REPORT_EVERY: u64 = 250;

    /// Record when a frame reached the transport, to measure how evenly they are paced.
    ///
    /// A healthy stream is one frame every 20ms. Clumps (gap ~0) followed by a long gap
    /// mean the receiver is getting bursts, not a stream.
    fn record_pacing(&mut self, now: std::time::Instant) {
        if let Some(previous) = self.last_frame_at {
            let gap = now.saturating_duration_since(previous);
            let gap_ms = u64::try_from(gap.as_millis()).unwrap_or(u64::MAX);
            self.max_gap_ms = self.max_gap_ms.max(gap_ms);
            if gap < std::time::Duration::from_millis(5) {
                self.burst_frames = self.burst_frames.saturating_add(1);
            }
        }
        self.last_frame_at = Some(now);
    }

    /// Record one encoded packet against the window's size statistics.
    fn record_packet(&mut self, len: usize, input_peak: f32) {
        self.bytes_since_report = self.bytes_since_report.saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
        if len <= SILENT_PACKET_BYTES {
            self.silent_packets_since_report = self.silent_packets_since_report.saturating_add(1);
            if input_peak > LOUD_INPUT_PEAK {
                self.loud_but_silent_since_report =
                    self.loud_but_silent_since_report.saturating_add(1);
            }
        }
    }

    /// Actual transmitted bitrate over this window, in kbps.
    const fn window_kbps(&self) -> u64 {
        // REPORT_EVERY frames of 20ms = 5s of audio.
        self.bytes_since_report.saturating_mul(8) / 1000 / 5
    }

    /// Emit a periodic summary, and reset the windowed peak.
    fn maybe_report(&mut self, sample_rate: u32, opus_rate: u32) {
        if self.captured == 1 || self.captured.is_multiple_of(Self::REPORT_EVERY) {
            tracing::info!(
                captured_frames = self.captured,
                encoded_frames = self.encoded,
                sent_frames = self.sent,
                skipped_not_connected = self.skipped_not_connected,
                dropped_channel_full = self.dropped_channel_full,
                encode_errors = self.encode_errors,
                input_peak_amplitude = self.peak_since_report,
                last_encoded_len = self.last_encoded_len,
                applied_packet_loss_perc = self.applied_packet_loss_perc,
                kbps = self.window_kbps(),
                silent_packets = self.silent_packets_since_report,
                loud_but_silent = self.loud_but_silent_since_report,
                max_gap_ms = self.max_gap_ms,
                burst_frames = self.burst_frames,
                capture_rate = sample_rate,
                opus_rate,
                "Outbound audio pipeline"
            );
            self.peak_since_report = 0.0;
            self.bytes_since_report = 0;
            self.silent_packets_since_report = 0;
            self.loud_but_silent_since_report = 0;
            self.max_gap_ms = 0;
            self.burst_frames = 0;
        }
    }
}

/// Everything the per-frame encode step needs, bundled so it stays one argument.
struct FrameEncodeCtx<'a> {
    encoder: &'a mut OpusEncoder,
    loopback_decoder: Option<&'a mut elementium_codec::OpusDecoder>,
    opus_rate: u32,
    channels: u16,
    frame_samples: usize,
}

/// Encode one 20ms frame and hand it to the peer connection, updating counters.
fn encode_and_send_frame(
    ctx: &mut FrameEncodeCtx<'_>,
    frame_data: Vec<f32>,
    encode_tx: &Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
    stats: &mut OutboundAudioStats,
) {
    stats.captured = stats.captured.saturating_add(1);

    // Peak of the raw captured samples, before encoding. This is the one measurement that
    // separates "the pipeline is broken" from "the microphone is delivering silence" --
    // everything downstream of a silent mic looks perfectly healthy while the far end
    // hears nothing.
    let peak = frame_data.iter().fold(0.0_f32, |acc, s| acc.max(s.abs()));
    stats.peak_since_report = stats.peak_since_report.max(peak);

    // Point 2: the exact 20ms frame about to be encoded, after any resampling and
    // reframing. Dumped regardless of whether a peer connection is attached, so the
    // microphone can be checked without needing to be in a call.
    elementium_media::audio_debug_dump::maybe_dump(
        "capture-encoder-in",
        ctx.opus_rate,
        ctx.channels,
        &frame_data,
    );

    // Only encode and send if connected to a peer connection
    if !encode_tx.lock().is_ok_and(|g| g.is_some()) {
        stats.skipped_not_connected = stats.skipped_not_connected.saturating_add(1);
        return;
    }

    let audio_frame = AudioFrame {
        sample_rate: ctx.opus_rate,
        channels: ctx.channels,
        data: frame_data,
        timestamp_us: 0,
    };

    match ctx.encoder.encode(&audio_frame) {
        Ok(encoded_frame) => {
            stats.encoded = stats.encoded.saturating_add(1);
            stats.last_encoded_len = encoded_frame.len();
            stats.record_packet(encoded_frame.len(), peak);
            stats.record_pacing(std::time::Instant::now());
            dump_loopback(
                ctx.loopback_decoder.as_deref_mut(),
                &encoded_frame,
                ctx.frame_samples,
            );
            if let Ok(guard) = encode_tx.lock()
                && let Some(ref tx) = *guard
            {
                if tx.try_send(IoCommand::WriteAudio(encoded_frame)).is_ok() {
                    stats.sent = stats.sent.saturating_add(1);
                } else {
                    stats.dropped_channel_full = stats.dropped_channel_full.saturating_add(1);
                }
            }
        }
        Err(e) => {
            stats.encode_errors = stats.encode_errors.saturating_add(1);
            tracing::debug!("Opus encode error: {e}");
        }
    }
}

/// Build the decoder used to record what our own encoder produces, if dumping is on.
///
/// Bisection of the outbound path, mirroring what `ELEMENTIUM_AUDIO_DUMP` already does for
/// inbound audio. Scalar telemetry cannot tell clean speech from garbage -- peak amplitude
/// looks the same either way -- so the samples themselves are written to disk at three
/// points, and comparing them localises any fault to one stage:
///
/// - `capture-raw`: what cpal handed us, untouched. Is the microphone healthy?
/// - `capture-encoder-in`: after resampling and reframing. Did we damage it?
/// - `capture-loopback`: encoded and decoded again. Is what we transmit intelligible?
///
/// The decoder exists only to produce that third file, so it is not built on a normal run.
fn make_loopback_decoder(rate: u32, channels: u16) -> Option<elementium_codec::OpusDecoder> {
    if !elementium_media::audio_debug_dump::is_enabled() {
        return None;
    }
    match elementium_codec::OpusDecoder::new(rate, channels) {
        Ok(d) => {
            tracing::info!("ELEMENTIUM_AUDIO_DUMP: capturing outbound audio at 3 bisection points");
            Some(d)
        }
        Err(e) => {
            tracing::warn!(error = %e, "ELEMENTIUM_AUDIO_DUMP: no loopback decoder; skipping the post-encode dump");
            None
        }
    }
}

/// Decode a just-encoded packet and dump the result: what the far end should hear.
///
/// The decisive step of the outbound bisection. If `capture-encoder-in` sounds fine but
/// this does not, the fault is ours (encoder settings, framing, channel count). If both
/// sound fine, what we transmit is good and any remaining distortion happened after us --
/// on the network or at the far end.
fn dump_loopback(
    decoder: Option<&mut elementium_codec::OpusDecoder>,
    packet: &elementium_types::PlaintextMedia,
    frame_samples: usize,
) {
    let Some(decoder) = decoder else {
        return;
    };
    match decoder.decode(packet, frame_samples) {
        Ok(pcm) => {
            elementium_media::audio_debug_dump::maybe_dump(
                "capture-loopback",
                pcm.sample_rate,
                pcm.channels,
                &pcm.data,
            );
        }
        Err(e) => {
            // Our own encoder produced something our own decoder rejects -- that alone
            // would explain unintelligible audio at the far end, so it is not silent.
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                tracing::error!(error = %e, "Loopback decode of our own Opus packet failed");
            });
        }
    }
}

/// Align the encoder's FEC sizing with the measured loss estimate.
///
/// Only acts when the whole-percent estimate actually moves. libopus reconfiguration is
/// cheap but not free, and more importantly the estimate is deliberately smoothed so that
/// this tracks real changes in link quality rather than per-report noise.
fn retune_fec_if_needed(
    encoder: &mut OpusEncoder,
    loss_estimate: &Arc<NetworkLossEstimate>,
    stats: &mut OutboundAudioStats,
) {
    let measured_loss = loss_estimate.percent();
    if measured_loss == stats.applied_packet_loss_perc {
        return;
    }
    match encoder.set_expected_packet_loss(measured_loss) {
        Ok(()) => {
            tracing::info!(
                previous_perc = stats.applied_packet_loss_perc,
                measured_perc = measured_loss,
                "Retuned Opus FEC from measured packet loss"
            );
            stats.applied_packet_loss_perc = measured_loss;
        }
        Err(e) => {
            tracing::warn!(
                measured_perc = measured_loss,
                error = %e,
                "Failed to retune Opus FEC; keeping previous setting"
            );
        }
    }
}

/// Background thread: captures mic audio, Opus-encodes, and sends to a peer
/// connection when `encode_tx` is connected (deferred connection pattern).
fn audio_capture_loop(
    encode_tx: &Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
    stop_rx: &std::sync::mpsc::Receiver<()>,
    loss_estimate: &Arc<NetworkLossEstimate>,
) {
    let capturer = match AudioCapturer::start() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to start audio capture: {e}");
            return;
        }
    };

    let sample_rate = capturer.sample_rate();
    let channels = capturer.channels();

    // Opus supports 8/12/16/24/48kHz; resample 44.1k → 48k
    let opus_rate = match sample_rate {
        8000 | 12000 | 16000 | 24000 | 48000 => sample_rate,
        _ => 48000,
    };

    let encoder_config = OpusEncoderConfig::default();
    let mut encoder = match OpusEncoder::with_config(opus_rate, OUTBOUND_CHANNELS, encoder_config) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("Failed to create Opus encoder: {e}");
            return;
        }
    };

    tracing::info!(
        sample_rate,
        channels,
        opus_rate,
        encoded_channels = OUTBOUND_CHANNELS,
        "Audio capture started"
    );

    // Opus frame size: 20ms at the given sample rate. `opus_rate` is always one of the
    // small fixed constants above, so these conversions/multiplications cannot overflow
    // or lose precision in practice.
    let channels_usize = usize::from(OUTBOUND_CHANNELS);
    let frame_samples = usize::try_from(opus_rate)
        .unwrap_or(48_000)
        .saturating_mul(20)
        / 1000;
    let frame_total_samples = frame_samples.saturating_mul(channels_usize);
    let mut accumulator: Vec<f32> = Vec::with_capacity(frame_total_samples.saturating_mul(2));

    // Outbound-path counters.
    //
    // Every failure in this loop used to be silent: frames dropped because no peer
    // connection was attached, encode errors at `debug`, and a discarded `try_send`
    // result. So "nobody can hear me" produced a completely clean log, and there was no
    // way to tell a dead microphone from a broken encoder from a full channel.
    let mut stats = OutboundAudioStats {
        applied_packet_loss_perc: encoder_config.expected_packet_loss_perc,
        ..OutboundAudioStats::default()
    };

    let mut loopback_decoder = make_loopback_decoder(opus_rate, OUTBOUND_CHANNELS);

    loop {
        if stop_rx.try_recv().is_ok() {
            tracing::info!(
                captured_frames = stats.captured,
                encoded_frames = stats.encoded,
                sent_frames = stats.sent,
                skipped_not_connected = stats.skipped_not_connected,
                dropped_channel_full = stats.dropped_channel_full,
                encode_errors = stats.encode_errors,
                "Audio capture stopping"
            );
            break;
        }

        if let Some(frame) = capturer.try_recv() {
            let mut data = frame.data;

            // Point 1: exactly what the microphone produced, before we touch it.
            elementium_media::audio_debug_dump::maybe_dump(
                "capture-raw",
                sample_rate,
                channels,
                &data,
            );

            // Simple sample rate conversion for 44.1kHz → 48kHz
            if sample_rate == 44100 && opus_rate == 48000 {
                data = elementium_media::audio_playback::resample_interleaved(
                    &data, channels, 44100, 48000,
                );
            }

            // Fold to mono before framing: the encoder, the RTP timeline and the SDP all
            // describe a single channel from here on. See `downmix_to_mono`.
            let mono = elementium_media::audio_capture::downmix_to_mono(&data, channels);
            accumulator.extend_from_slice(&mono);

            // Process complete Opus frames
            while accumulator.len() >= frame_total_samples {
                let frame_data: Vec<f32> = accumulator.drain(..frame_total_samples).collect();

                let mut ctx = FrameEncodeCtx {
                    encoder: &mut encoder,
                    loopback_decoder: loopback_decoder.as_mut(),
                    opus_rate,
                    channels: OUTBOUND_CHANNELS,
                    frame_samples,
                };
                encode_and_send_frame(&mut ctx, frame_data, encode_tx, &mut stats);

                retune_fec_if_needed(&mut encoder, loss_estimate, &mut stats);

                stats.maybe_report(sample_rate, opus_rate);
            }
        } else {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

fn generate_track_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{t:x}")
}


#[cfg(test)]
mod video_bitrate_tests {
    use super::{MAX_ENCODE_FPS, MIN_ENCODE_INTERVAL, bitrate_for};

    /// The regression this guards: a fixed 500kbps was used at every resolution. At 720p
    /// that is about a tenth of what the picture needs, and the encoder meets the budget
    /// by throwing away detail -- blocky, smeared output that looks like a transmission
    /// fault and is not one.
    #[test]
    fn bitrate_scales_with_resolution() {
        let vga = bitrate_for(640, 480);
        let hd = bitrate_for(1280, 720);

        assert!(
            hd > vga.saturating_mul(2),
            "720p has three times the pixels of VGA and must get far more than {vga}kbps, got {hd}"
        );
        assert!(
            hd >= 2000,
            "720p needs a bitrate in the megabits, got {hd}kbps"
        );
    }

    /// Absurd geometry must not produce an absurd bitrate in either direction.
    #[test]
    fn bitrate_is_clamped_at_both_ends() {
        assert_eq!(bitrate_for(1, 1), 300, "a tiny frame still needs a floor");
        assert_eq!(
            bitrate_for(7680, 4320),
            4000,
            "8K must not ask for hundreds of megabits"
        );
    }

    /// The encode interval must match the declared cap; deriving one from the other by
    /// hand is how they drift apart.
    #[test]
    fn the_encode_interval_matches_the_frame_rate_cap() {
        let per_second = 1_000_000_000_u64
            .checked_div(MIN_ENCODE_INTERVAL.as_nanos().try_into().unwrap_or(u64::MAX))
            .unwrap_or(0);
        assert_eq!(per_second, MAX_ENCODE_FPS);
    }
}
