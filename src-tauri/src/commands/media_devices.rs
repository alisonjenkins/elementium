// Every `#[tauri::command]` async fn below that takes a `State<'_, T>` parameter causes
// the `#[command]` macro to generate a sibling IPC-dispatch wrapper item in this module
// containing an internal match with an arm clippy flags as unreachable. That wrapper is
// framework codegen (not nested inside the fn item itself, so a function- or
// statement-scoped `#[allow]` cannot reach it — verified empirically), hence the
// module-level allow here rather than the usual per-item scoping.
#![allow(clippy::unreachable)]
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tauri::{State, command};
use tokio::sync::mpsc as tokio_mpsc;

use elementium_codec::{
    EncoderConfig, NegotiatedEncoder, OpusEncoder, OpusEncoderConfig, VideoCodec, VideoEncoder,
};
use elementium_media::audio_capture::AudioCapturer;
use elementium_media::device_enumeration;
use elementium_types::observability::CorrelationId;
use elementium_types::{
    AudioFrame, MediaConstraints, MediaDevice, MediaTrackKey, NetworkLossEstimate, TrackId,
};
use elementium_webrtc::engine::{IoCommand, VideoFrameBuffer};

use super::LockExt;
use super::webrtc::WebRtcState;
use crate::protocols::VideoFrameState;

/// The parts of a pipeline handle that only one kind of pipeline has.
///
/// An enum rather than a pile of `Option` fields so that "the camera's keyframe flag" and
/// "the microphone's loss estimate" cannot be asked for on the wrong pipeline and silently
/// answered `None`.
pub enum PipelineExtras {
    Audio {
        /// Measured outbound packet loss, fed from RTCP receiver reports.
        ///
        /// Written by whoever observes `PcEvent::EgressStats`, read by the capture thread
        /// to size the Opus encoder's FEC redundancy. Shared rather than passed through
        /// the command channel because it is a continuously-updated level, not an event:
        /// the capture thread only cares about the latest value.
        loss_estimate: Arc<NetworkLossEstimate>,
    },
    Video {
        /// Set when a receiver sends an RTCP keyframe request (PLI/FIR).
        ///
        /// A level rather than an event, like `loss_estimate` on the audio side: several
        /// receivers asking at once still means "one keyframe, now", and the encoder
        /// thread only cares about the latest state. Cleared when the keyframe is
        /// produced.
        keyframe_requested: Arc<std::sync::atomic::AtomicBool>,
        /// The codec the SFU currently wants from us, which can change mid-call.
        active_codec: Arc<ActiveCodec>,
    },
}

/// Handle to one running capture pipeline.
///
/// One type for microphone, camera and screen share, because everything that operates on a
/// running pipeline -- stopping it, attaching it to a call, handing its connection to a
/// replacement -- is the same operation regardless of what it captures. The differences
/// live in [`PipelineExtras`].
pub struct PipelineHandle {
    /// Which of the user's tracks this pipeline feeds. Its identity everywhere.
    pub key: MediaTrackKey,
    pub track_id: String,
    pub stop_tx: std::sync::mpsc::Sender<()>,
    /// Set to enable encoding and sending to a peer connection.
    /// When `None`, the pipeline captures but does not encode or send.
    pub encode_tx: Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
    pub extras: PipelineExtras,
}

impl PipelineHandle {
    /// The keyframe flag and codec, if this is a video pipeline.
    #[must_use]
    pub const fn video(&self) -> Option<(&Arc<std::sync::atomic::AtomicBool>, &Arc<ActiveCodec>)> {
        match &self.extras {
            PipelineExtras::Video {
                keyframe_requested,
                active_codec,
            } => Some((keyframe_requested, active_codec)),
            PipelineExtras::Audio { .. } => None,
        }
    }

    /// The measured outbound loss, if this is an audio pipeline.
    #[must_use]
    pub const fn loss_estimate(&self) -> Option<&Arc<NetworkLossEstimate>> {
        match &self.extras {
            PipelineExtras::Audio { loss_estimate } => Some(loss_estimate),
            PipelineExtras::Video { .. } => None,
        }
    }
}

/// State for active media tracks (audio capture, video capture, etc.).
pub struct MediaState {
    pub active_tracks: Mutex<Vec<TrackId>>,
    /// Every running capture pipeline, by the track it feeds.
    ///
    /// Was two `Option` slots, one for the camera and one for the microphone. That shape
    /// could not express a user sharing their screen while their camera is on -- the share
    /// would have had to evict the camera -- and a call UI showing a participant's camera
    /// replaced by their screen, rather than alongside it, is visibly wrong.
    ///
    /// At most one pipeline per key, which keeps "two cameras" impossible while making
    /// "camera and screen" expressible.
    pub pipelines: Mutex<HashMap<MediaTrackKey, PipelineHandle>>,
    /// Where captured media goes for the currently-connected SFU room, if any.
    ///
    /// Publishing a track attaches whatever pipeline is running at that moment, but the
    /// two events have no fixed order: joining a call muted and unmuting later starts the
    /// microphone long after its track was published, and that pipeline has no earlier
    /// handle to inherit a connection from. Remembering the room's sender here means a
    /// pipeline that starts at any point in a call is attached on startup.
    pub sfu_media_tx: Mutex<Option<tokio_mpsc::Sender<IoCommand>>>,
    /// The screen share currently running, if any.
    ///
    /// Held separately from `pipelines` because a share owns something a capture pipeline
    /// does not: a portal session, which has to be closed when the share ends. Keeping it
    /// here is what makes teardown complete rather than approximate -- the previous code
    /// dropped every handle to the session at the end of the function that created it, so
    /// nothing ever told the portal the share was over.
    pub share: Mutex<Option<ShareHandle>>,
}

/// A running screen share: the track it feeds, and the portal session behind it.
pub struct ShareHandle {
    pub track_id: String,
    pub session: elementium_screen::ShareSession,
}

/// Start a video pipeline fed by a portal-granted screencast node.
///
/// The camera equivalent lives inside `get_user_media`; this is the same sequence with the
/// device-release wait left out, since a screencast node has no exclusive hardware to free.
///
/// # Errors
///
/// Returns a description if the pipeline could not be recorded in [`MediaState`].
pub fn start_screen_share_pipeline(
    media_state: &MediaState,
    video_frames: &VideoFrameBuffer,
    session: &elementium_screen::ShareSession,
) -> Result<String, String> {
    let key = MediaTrackKey::screen_share();
    let track_id = format!("video-{}", generate_track_id());

    // Same inheritance as the camera path: a share restarted mid-call keeps feeding the
    // connection it was already attached to, rather than going quiet until the next
    // renegotiation happens to occur.
    let previous = stop_pipeline_inheriting_connection(&media_state.pipelines, key);
    let connection = connection_for_new_pipeline(previous.connection, media_state);

    let encode_tx: Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>> =
        Arc::new(Mutex::new(connection));
    let encode_tx_clone = encode_tx.clone();
    let keyframe_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let keyframe_requested_clone = keyframe_requested.clone();
    let active_codec = Arc::new(ActiveCodec::new(DEFAULT_VIDEO_CODEC));
    let active_codec_clone = active_codec.clone();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();

    // Which capture source to open is decided by which backend granted the share -- a
    // PipeWire node to pull for the portal, a source id to hand X11's push capturer for
    // X11 -- not by anything the pipeline loop itself has to know about.
    let capture_source = match session.source() {
        elementium_screen::ShareSource::Wayland { node_id } => {
            VideoCaptureSource::Screencast { node_id: *node_id }
        }
        elementium_screen::ShareSource::X11 { source_id } => {
            VideoCaptureSource::X11 { source_id: source_id.clone() }
        }
    };

    let tid = track_id.clone();
    let frames = video_frames.clone();
    let span = tracing::Span::current();
    std::thread::spawn(move || {
        let _guard = span.enter();
        video_pipeline_loop(
            key,
            &tid,
            &capture_source,
            &frames,
            &encode_tx_clone,
            &keyframe_requested_clone,
            &active_codec_clone,
            &stop_rx,
        );
    });

    media_state
        .pipelines
        .lock()
        .map_err(|_| "pipeline map lock poisoned".to_owned())?
        .insert(
            key,
            PipelineHandle {
                key,
                track_id: track_id.clone(),
                stop_tx,
                encode_tx,
                extras: PipelineExtras::Video {
                    keyframe_requested,
                    active_codec,
                },
            },
        );

    if let Ok(mut tracks) = media_state.active_tracks.lock() {
        tracks.push(TrackId(track_id.clone()));
    }

    Ok(track_id)
}

/// Start a pipeline capturing share audio from a specific `PipeWire` node, keyed
/// `MediaTrackKey::screen_share_audio()`.
///
/// The audio counterpart to [`start_screen_share_pipeline`]: same registration in
/// [`MediaState::pipelines`], same connection-inheriting restart as every other capture
/// pipeline here (see [`stop_pipeline_inheriting_connection`]), but reading from a node the
/// caller has already chosen rather than one this function discovers -- node selection is
/// [`super::screen_capture::start_share_audio`]'s job, since it is the one with the fallback
/// policy to apply.
///
/// # Errors
///
/// Returns a description if the pipeline could not be recorded in [`MediaState`].
pub fn start_screen_share_audio_pipeline(
    media_state: &MediaState,
    node_id: u32,
    source_kind: elementium_media::pipewire_audio::AudioSourceKind,
) -> Result<String, String> {
    let key = MediaTrackKey::screen_share_audio();
    let track_id = format!("audio-{}", generate_track_id());

    let previous = stop_pipeline_inheriting_connection(&media_state.pipelines, key);
    let connection = connection_for_new_pipeline(previous.connection, media_state);

    let encode_tx: Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>> =
        Arc::new(Mutex::new(connection));
    let encode_tx_clone = encode_tx.clone();
    // Seeded the same way the microphone's is; nothing here retunes it from RTCP, so it
    // stays at the configured default for the pipeline's lifetime rather than drifting.
    let loss_estimate = Arc::new(NetworkLossEstimate::new(
        OpusEncoderConfig::default().expected_packet_loss_perc,
    ));
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();

    let span = tracing::Span::current();
    std::thread::spawn(move || {
        let _guard = span.enter();
        screen_share_audio_capture_loop(key, node_id, source_kind, &encode_tx_clone, &stop_rx);
    });

    media_state
        .pipelines
        .lock()
        .map_err(|_| "pipeline map lock poisoned".to_owned())?
        .insert(
            key,
            PipelineHandle {
                key,
                track_id: track_id.clone(),
                stop_tx,
                encode_tx,
                extras: PipelineExtras::Audio { loss_estimate },
            },
        );

    if let Ok(mut tracks) = media_state.active_tracks.lock() {
        tracks.push(TrackId(track_id.clone()));
    }

    Ok(track_id)
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
    let key = MediaTrackKey::microphone();
    let track_id = TrackId(format!("audio-{}", generate_track_id()));
    tracing::info!(track_id = %track_id, "Starting audio capture");

    let previous =
        stop_pipeline_inheriting_connection(&media_state.pipelines, MediaTrackKey::microphone());
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
        audio_capture_loop(key, &encode_tx_clone, &stop_rx, &loss_estimate_clone);
    });

    if let Ok(mut pipelines) = media_state.pipelines.lock() {
        pipelines.insert(
            MediaTrackKey::microphone(),
            PipelineHandle {
                key: MediaTrackKey::microphone(),
                track_id: track_id.0.clone(),
                stop_tx,
                encode_tx,
                extras: PipelineExtras::Audio { loss_estimate },
            },
        );
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
fn stop_pipeline_inheriting_connection(
    pipelines: &Mutex<HashMap<MediaTrackKey, PipelineHandle>>,
    key: MediaTrackKey,
) -> StoppedPipeline {
    let Ok(mut guard) = pipelines.lock() else {
        return StoppedPipeline {
            existed: false,
            connection: None,
        };
    };
    let Some(old) = guard.remove(&key) else {
        return StoppedPipeline {
            existed: false,
            connection: None,
        };
    };
    let _ = old.stop_tx.send(());
    let connection = old.encode_tx.lock().ok().and_then(|c| c.clone());
    StoppedPipeline {
        existed: true,
        connection,
    }
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
        let previous =
            stop_pipeline_inheriting_connection(&media_state.pipelines, MediaTrackKey::camera());
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
        // Honour the caller's frame rate: a call wants 30, streaming wants 60 or more.
        // Asking for what will be consumed means the surplus is never decoded.
        let req_fps = video_constraints
            .frame_rate
            .map_or(MAX_ENCODE_FPS_U32, requested_fps);

        let encode_tx: Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>> =
            Arc::new(Mutex::new(inherited_connection));
        let encode_tx_clone = encode_tx.clone();
        let keyframe_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let keyframe_requested_clone = keyframe_requested.clone();
        let active_codec = Arc::new(ActiveCodec::new(DEFAULT_VIDEO_CODEC));
        let active_codec_clone = active_codec.clone();
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
            video_pipeline_loop(
                MediaTrackKey::camera(),
                &tid,
                &VideoCaptureSource::Camera {
                    width: req_width,
                    height: req_height,
                    fps: req_fps,
                },
                &video_frames,
                &encode_tx_clone,
                &keyframe_requested_clone,
                &active_codec_clone,
                &stop_rx,
            );
        });

        // Store the camera pipeline handle
        if let Ok(mut pipelines) = media_state.pipelines.lock() {
            pipelines.insert(
                MediaTrackKey::camera(),
                PipelineHandle {
                    key: MediaTrackKey::camera(),
                    track_id: track_id.0.clone(),
                    stop_tx,
                    encode_tx,
                    extras: PipelineExtras::Video {
                        keyframe_requested,
                        active_codec,
                    },
                },
            );
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

    // Found by the track id the caller holds rather than by guessing the pipeline from the
    // id's prefix: with more than one pipeline of a kind running, "starts with video-" no
    // longer identifies which one, and stopping a share would have stopped the camera.
    if let Ok(mut pipelines) = media_state.pipelines.lock()
        && let Some(key) = pipelines
            .iter()
            .find(|(_, h)| h.track_id == track_id.0)
            .map(|(k, _)| *k)
        && let Some(handle) = pipelines.remove(&key)
    {
        let _ = handle.stop_tx.send(());
        tracing::info!(%track_id, %key, "capture pipeline stopped");
    }

    // A share is one thing to the user, so it is one teardown: stopping its video track
    // must also close the portal session, or the compositor goes on believing the share is
    // live and the indicator stays lit after the call has moved on.
    //
    // Taken from under the lock before awaiting, for the same Send reason as in
    // `get_display_media`.
    let ended_share = media_state
        .share
        .lock()
        .ok()
        .and_then(|mut slot| slot.take_if(|s| s.track_id == track_id.0));
    if let Some(share) = ended_share {
        // The share's audio pipeline is stopped by key, not by hunting for its track id: the
        // page may only ever have called stop() on the video track (the contract requires
        // that alone to be enough), so the audio pipeline the loop above searched for by
        // `track_id` a moment ago may still be sitting in the map under its own id.
        if let Ok(mut pipelines) = media_state.pipelines.lock()
            && let Some(audio_handle) = pipelines.remove(&MediaTrackKey::screen_share_audio())
        {
            let _ = audio_handle.stop_tx.send(());
            tracing::info!(
                audio_track_id = %audio_handle.track_id,
                "share audio pipeline stopped with its share"
            );
        }
        share.session.close().await;
        tracing::info!(%track_id, "screen share torn down");
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

/// Presence of this file turns preview dumping on, without restarting the app.
///
/// An environment variable is the wrong switch for a desktop app: the process is usually
/// started by a dev server or a desktop launcher that was itself started long before, so
/// exporting a variable in a terminal does not reach it. That is exactly what happened on
/// the first attempt to use this -- the dump was compiled in, running, and silently
/// disabled, which is the same failure mode as the bugs it exists to find.
const DUMP_PREVIEW_SENTINEL: &str = "/tmp/elementium-dump-preview";

/// Write a preview frame to disk while dumping is enabled.
///
/// Settles a question that cannot be answered from either end alone: whether a corrupt
/// self-view is corrupt in the pixels Rust produces, or only after they have crossed into
/// the webview and been drawn to a canvas. The camera probe shows the capture path clean
/// and the preview shows torn output; exactly one of the steps between them is
/// responsible, and reading the code has not settled which.
///
/// Enabled by `ELEMENTIUM_DUMP_PREVIEW` or by creating [`DUMP_PREVIEW_SENTINEL`].
///
/// Raw RGBA with the geometry in the filename, because writing a PNG encoder here to
/// inspect one frame is not worth it -- `ffmpeg -f rawvideo -pix_fmt rgba -s WxH` reads it.
fn maybe_dump_preview(frame_count: u64, rgba: &[u8], width: u32, height: u32) {
    if !frame_count.is_multiple_of(60) {
        return;
    }
    if std::env::var_os("ELEMENTIUM_DUMP_PREVIEW").is_none()
        && !std::path::Path::new(DUMP_PREVIEW_SENTINEL).exists()
    {
        return;
    }
    let path = format!("/tmp/elementium_preview_{frame_count}_{width}x{height}.rgba");
    match std::fs::write(&path, rgba) {
        Ok(()) => tracing::info!(
            path,
            width,
            height,
            bytes = rgba.len(),
            "preview frame dumped"
        ),
        Err(e) => tracing::warn!(path, reason = %e, "could not dump preview frame"),
    }
}

/// How long a newly-subscribed peer may wait before it can decode anything.
///
/// Short enough that joining a call feels immediate, long enough that the cost of
/// rebuilding the encoder's rate control is negligible.
const KEYFRAME_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// The shortest gap between two keyframes, however many receivers ask.
///
/// A keyframe costs many times an interframe, and a receiver cannot benefit from a second
/// one before the first has arrived. Roughly one round trip on a poor link.
const MIN_KEYFRAME_GAP: std::time::Duration = std::time::Duration::from_millis(500);

/// The codec used for outbound video until negotiation says otherwise.
///
/// VP8 because it is the one every peer speaks. The live value is carried by
/// [`ActiveCodec`], which the SFU can change mid-call.
const DEFAULT_VIDEO_CODEC: VideoCodec = VideoCodec::Vp8;

/// The codec the capture pipeline should currently be encoding with.
///
/// Shared rather than passed, and mutable during a call, because the answer changes while
/// the call is running. A room where everyone supports AV1 gains a participant who does
/// not: the SFU does not transcode, so either that participant sees nothing or the
/// publisher regresses to a codec they can decode. `LiveKit` signals this through
/// `BackupCodecPolicy` and `SubscribedCodec`, and the encoder has to be able to follow.
///
/// A level rather than an event, like the keyframe request: several notifications still
/// mean "encode with this now", and the capture thread only cares about the latest value.
#[derive(Debug)]
pub struct ActiveCodec(std::sync::atomic::AtomicU8);

impl ActiveCodec {
    /// Start with `codec`.
    #[must_use]
    pub const fn new(codec: VideoCodec) -> Self {
        Self(std::sync::atomic::AtomicU8::new(Self::encode(codec)))
    }

    /// The codec to encode with now.
    #[must_use]
    pub fn get(&self) -> VideoCodec {
        Self::decode(self.0.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Ask for a different codec from the next frame on.
    pub fn set(&self, codec: VideoCodec) {
        self.0
            .store(Self::encode(codec), std::sync::atomic::Ordering::Relaxed);
    }

    /// `VideoCodec` as an atom. An explicit mapping rather than a cast, so adding a codec
    /// is a compile error here rather than a value that silently decodes as another.
    const fn encode(codec: VideoCodec) -> u8 {
        match codec {
            VideoCodec::Vp8 => 0,
            VideoCodec::H264 => 1,
            VideoCodec::Av1 => 2,
        }
    }

    /// Inverse of [`ActiveCodec::encode`]. Anything unrecognised falls back to the codec
    /// every peer speaks, because a wrong codec is worse than a slow one.
    const fn decode(raw: u8) -> VideoCodec {
        match raw {
            1 => VideoCodec::H264,
            2 => VideoCodec::Av1,
            _ => DEFAULT_VIDEO_CODEC,
        }
    }
}

/// The fastest we encode, regardless of how fast the camera runs.
///
/// The webcam delivers 60fps. Encoding all of it doubles the bitrate needed for the same
/// picture quality and doubles the CPU cost, for a difference nobody watching a video call
/// can see. The preview still shows every captured frame; only the encoder skips.
const MAX_ENCODE_FPS: u64 = 30;

/// How often the self-view is recomputed, however fast the camera runs.
///
/// The preview is a thumbnail a few centimetres across, fetched by the webview about
/// thirty times a second. Recomputing it per captured frame means a downscale and a colour
/// conversion per frame that nothing reads: at 240fps capture, seven eighths of that work
/// is discarded before anyone sees it. The encoder has always been rate-limited; the
/// preview was not.
const PREVIEW_FPS: u64 = 30;

/// Minimum gap between preview updates, from [`PREVIEW_FPS`].
const MIN_PREVIEW_INTERVAL: std::time::Duration =
    std::time::Duration::from_nanos(1_000_000_000 / PREVIEW_FPS);

/// [`MAX_ENCODE_FPS`] as the width the capture API takes.
const MAX_ENCODE_FPS_U32: u32 = 30;

/// Turn a `getUserMedia` frame-rate constraint into a rate to ask the camera for.
///
/// Clamped rather than trusted: a source asked for zero delivers nothing, and an absurd
/// rate would have us decoding frames no encoder or display will ever consume. The upper
/// bound is generous because high-rate capture is a real use -- streaming and screen
/// capture want more than a call does.
fn requested_fps(constraint: f64) -> u32 {
    // The rates worth asking a camera for. Walking these rather than converting the float
    // avoids a cast the lints reject for good reason, and a camera offers a handful of
    // rates in any case.
    const RATES: [u32; 8] = [1, 5, 10, 15, 24, 30, 60, 120];

    if !constraint.is_finite() {
        return MAX_ENCODE_FPS_U32;
    }
    let clamped = constraint.round().clamp(1.0, 240.0);
    // Nearest offered rate, and on a tie the lower one -- `RATES` is ascending and
    // `min_by` keeps the first of equal elements. Ties break downwards deliberately:
    // exceeding what was asked for spends CPU and bitrate nobody requested.
    RATES
        .iter()
        .copied()
        .min_by(|a, b| {
            let (da, db) = (
                (f64::from(*a) - clamped).abs(),
                (f64::from(*b) - clamped).abs(),
            );
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(MAX_ENCODE_FPS_U32)
}

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

/// What a video pipeline is capturing, and what it needs to open it.
///
/// The rest of the pipeline -- encoder negotiation, keyframe policy, pacing, preview,
/// E2EE-bound dispatch -- is identical for a camera and a shared screen, so the difference
/// is confined to this one enum rather than to a second copy of the loop.
#[derive(Debug, Clone)]
pub enum VideoCaptureSource {
    /// A camera, found by enumeration.
    Camera {
        width: Option<u32>,
        height: Option<u32>,
        fps: u32,
    },
    /// A `PipeWire` node the desktop portal already granted for screen capture.
    Screencast { node_id: u32 },
    /// An X11 source id (`monitor-<n>`/`window-<n>` from `get_capture_sources`), captured by
    /// `xcap` through the push adapter -- see `start_x11_video_source` and
    /// `elementium_media::video_source::VideoSource::start_push`.
    X11 { source_id: String },
}

impl VideoCaptureSource {
    /// The geometry to negotiate an encoder against, before the source has reported its
    /// own.
    ///
    /// A share has no requested size -- the user picked a monitor or a window and its size
    /// is whatever it is -- so 1920x1080 stands in until the first frame says otherwise.
    /// This decides only whether a hardware encoder is size-capable, not what is captured.
    /// Not `String`-holding data itself, so borrowing rather than consuming `self` costs
    /// nothing here and lets `open`/`label` keep doing the same below.
    const fn negotiation_geometry(&self) -> (u32, u32) {
        match self {
            Self::Camera { width, height, .. } => {
                (unwrap_or_const(*width, 1280), unwrap_or_const(*height, 720))
            }
            Self::Screencast { .. } | Self::X11 { .. } => (1920, 1080),
        }
    }

    /// Open the source.
    fn open(
        &self,
        target: elementium_codec::EncodeTarget,
    ) -> Result<elementium_media::video_source::VideoSource, String> {
        use elementium_media::video_source::VideoSource;
        match self {
            Self::Camera { width, height, fps } => VideoSource::start_at(*width, *height, *fps, target),
            Self::Screencast { node_id } => VideoSource::start_screencast(*node_id, target),
            Self::X11 { source_id } => start_x11_video_source(source_id),
        }
    }

    /// What to call this in a log line.
    const fn label(&self) -> &'static str {
        match self {
            Self::Camera { .. } => "camera",
            Self::Screencast { .. } => "screencast",
            Self::X11 { .. } => "x11",
        }
    }
}

/// Bridge a running `X11Capturer` into a `VideoSource`.
///
/// `elementium-media` cannot depend on `elementium-screen` (the dependency runs the other
/// way: `elementium-screen` depends on `elementium-media` for `I420Frame` conversion), so
/// `VideoSource::start_push` is generic over any push producer and this is where the
/// concrete `X11Capturer` meets it -- the only place in the codebase that imports both.
///
/// `target` (the negotiated encoder geometry) is not passed to `X11Capturer::start`: xcap
/// captures whatever the monitor or window actually is and has no format to negotiate,
/// unlike the `PipeWire` path where the capture format itself depends on where the frame is
/// headed.
///
/// The capturer is parked behind a mutex only so its `stop(&mut self)` can be reached from
/// the `Fn` (not `FnMut`) closure `VideoSource::start_push` requires; this is not a hot
/// path, so the extra indirection costs nothing that matters.
///
/// # Errors
///
/// Whatever `X11Capturer::start` reports: no display, an unrecognised or non-existent
/// source id, or a backend failure (see `x11.rs`'s four distinct messages).
fn start_x11_video_source(
    source_id: &str,
) -> Result<elementium_media::video_source::VideoSource, String> {
    use elementium_media::captured_frame::CapturedFrame;
    use elementium_screen::ScreenCapturer;
    use elementium_screen::x11::X11Capturer;

    let mut capturer = X11Capturer::new();
    let (tx, rx) = std::sync::mpsc::channel();
    capturer
        .start(
            source_id,
            Box::new(move |frame| {
                let _ = tx.send(CapturedFrame::Planar(frame));
            }),
        )
        .map_err(|e| e.to_string())?;

    let capturer = Arc::new(Mutex::new(capturer));
    let stopper_capturer = Arc::clone(&capturer);
    let stopper: Box<dyn Fn() + Send + Sync> = Box::new(move || {
        if let Ok(mut c) = stopper_capturer.lock() {
            let _ = c.stop();
        }
    });

    Ok(elementium_media::video_source::VideoSource::start_push(
        rx, stopper, "x11",
    ))
}

/// `Option::unwrap_or` is not `const`; this is.
const fn unwrap_or_const(value: Option<u32>, fallback: u32) -> u32 {
    match value {
        Some(v) => v,
        None => fallback,
    }
}

/// Drop this pipeline's self-view entry, so a stopped or dead source stops feeding it.
///
/// Shared by the stop path and the capture-failure path below, which have to leave exactly
/// the same state behind: a preview that outlives its source shows a frozen last frame,
/// which reads as "the share is still running" to anyone looking at it.
fn release_preview(video_frames: &VideoFrameBuffer, track_id: &str) {
    if let Ok(mut buf) = video_frames.lock() {
        buf.remove(track_id);
    }
}

/// Background thread: reads frames from a video source, writes RGBA to `VideoFrameBuffer`
/// for preview, and optionally encodes + sends them to a peer connection.
///
/// Serves the camera and the screen share both. They differ in how the source is opened and
/// in nothing else, so a second copy of this loop would only be a second place for every
/// future encode fix to be applied -- or forgotten.
#[allow(clippy::too_many_arguments)]
fn video_pipeline_loop(
    key: MediaTrackKey,
    track_id: &str,
    source: &VideoCaptureSource,
    video_frames: &VideoFrameBuffer,
    encode_tx: &Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
    keyframe_requested: &Arc<std::sync::atomic::AtomicBool>,
    active_codec: &Arc<ActiveCodec>,
    stop_rx: &std::sync::mpsc::Receiver<()>,
) {
    // Which format is worth asking the source for depends on where the frames will be
    // encoded, and the answer is not a small difference: MJPEG is the most expensive format
    // on offer when the CPU decodes it and the cheapest when the GPU does.
    let (neg_width, neg_height) = source.negotiation_geometry();
    let target = elementium_codec::EncodeTarget::negotiated(active_codec.get(), neg_width, neg_height);
    // Once per pipeline, so "is the GPU doing this" is answerable by reading a log rather
    // than by re-deriving the selection policy from the codec, the geometry and what the
    // driver reported. The three fields are what the policy actually decided on, and
    // `capture_preference` is what it will ask the source for as a result.
    tracing::info!(
        codec = active_codec.get().sdp_name(),
        backend = ?target.backend,
        layout = ?target.layout,
        gpu_jpeg_decode = target.gpu_jpeg_decode,
        capture_preference = ?elementium_codec::capture_format::preference(target),
        track_id = %track_id,
        source = source.label(),
        "video encode target negotiated"
    );
    let capturer = match source.open(target) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                reason = %e,
                track_id = %track_id,
                source = source.label(),
                "failed to start video capture"
            );
            return;
        }
    };

    let (width, height) = capturer.size();
    tracing::info!(
        width,
        height,
        backend = capturer.backend(),
        track_id = %track_id,
        source = source.label(),
        "video pipeline started"
    );

    // The codec comes from SDP negotiation, so the capture loop must not name one. Held
    // as `NegotiatedEncoder` rather than a trait object: dispatch stays static on a path
    // that runs thirty times a second. See `elementium_codec::video`.
    let mut encoder: Option<NegotiatedEncoder> = None;
    let mut frame_count: u64 = 0;
    let mut last_keyframe = std::time::Instant::now();
    let mut last_preview = std::time::Instant::now()
        .checked_sub(MIN_PREVIEW_INTERVAL)
        .unwrap_or_else(std::time::Instant::now);
    let mut last_encode = std::time::Instant::now()
        .checked_sub(MIN_ENCODE_INTERVAL)
        .unwrap_or_else(std::time::Instant::now);

    loop {
        if stop_rx.try_recv().is_ok() {
            tracing::info!(track_id = %track_id, "video pipeline stopping");
            release_preview(video_frames, track_id);
            break;
        }

        // A shared window closing, or a monitor being unplugged, errors the stream rather
        // than ending it: `try_recv` simply keeps returning `None`, which is also what a
        // perfectly healthy screencast of a static window does between damage events.
        // Without this check the pipeline spins forever on a dead source, publishing
        // nothing, logging nothing, and looking to every counter like an idle share.
        if capturer.failed() {
            tracing::error!(
                track_id = %track_id,
                source = source.label(),
                frame_count,
                "video capture failed; the source is gone and the pipeline is stopping"
            );
            release_preview(video_frames, track_id);
            break;
        }

        if let Some(frame) = capturer.try_recv() {
            frame_count = frame_count.wrapping_add(1);
            if frame_count <= 3 || frame_count.is_multiple_of(100) {
                tracing::info!(
                    track_id = %track_id,
                    frame_count,
                    w = frame.width(),
                    h = frame.height(),
                    compressed = frame.mjpeg().is_some(),
                    "Camera frame received"
                );
            }
            // The self-view: halved, then converted to RGBA for the canvas. Rate-limited
            // independently of capture, because nothing consumes it faster.
            //
            // Halving first is the cheap order. I420 is 1.5 bytes per pixel against RGBA's
            // 4, so the downscale touches under 40% of the memory, and the conversion that
            // follows then runs on a quarter of the pixels. Doing it the other way round
            // converts the full frame and throws three quarters of it away.
            //
            // Every preview frame crosses the Rust-to-webview boundary as raw RGBA, so
            // halving also cuts that traffic fourfold. What peers receive is still the
            // full-resolution frame -- only the self-view is reduced.
            if last_preview.elapsed() >= MIN_PREVIEW_INTERVAL {
                last_preview = std::time::Instant::now();
                // `to_preview` halves a decoded frame and decodes a compressed one at half
                // scale, which is where the rate limit earns its keep: on the accelerated
                // path most frames reach the encoder without ever being decoded, and only
                // the ones actually displayed cost anything.
                if let Some(half) = frame.to_preview() {
                    let preview = elementium_codec::i420_to_rgba(&half);
                    maybe_dump_preview(frame_count, &preview.data, preview.width, preview.height);
                    if let Ok(mut buf) = video_frames.lock() {
                        buf.insert(track_id.to_string(), preview);
                    }
                }
            }

            // VP8 encode and send if encoding is active
            let should_encode = encode_tx.lock().is_ok_and(|g| g.is_some());

            // The preview above gets every captured frame; the encoder is rate-limited.
            if should_encode && last_encode.elapsed() >= MIN_ENCODE_INTERVAL {
                last_encode = std::time::Instant::now();
                encode_and_send_video_frame(
                    PipelineId { key, track_id },
                    &frame,
                    &mut encoder,
                    &mut last_keyframe,
                    keyframe_requested,
                    active_codec,
                    encode_tx,
                );
            }
        } else {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

/// Keep `encoder` valid for `frame`, creating or replacing it when the geometry changes.
///
/// Split from the encode step so that step can be written against a trait bound rather
/// than a concrete type: construction is the one place the negotiated codec has to be
/// named, and it has no business being on the per-frame path.
fn ensure_encoder(
    encoder: &mut Option<NegotiatedEncoder>,
    width: u32,
    height: u32,
    last_keyframe: &mut std::time::Instant,
    wanted: VideoCodec,
) {
    // Rebuilt when the geometry changes, because an encoder rejects a frame whose size
    // does not match its configuration and would otherwise fail every frame from then on;
    // and when the codec changes, because the SFU can ask us to regress mid-call once a
    // participant joins who cannot decode what everyone else could.
    if encoder
        .as_ref()
        .is_some_and(|e| e.size() == (width, height) && e.codec() == wanted)
    {
        return;
    }

    let bitrate = bitrate_for(width, height);
    let config = EncoderConfig {
        width,
        height,
        bitrate_kbps: bitrate,
        max_framerate: u32::try_from(MAX_ENCODE_FPS).unwrap_or(30),
    };
    match NegotiatedEncoder::new(wanted, config) {
        Ok(enc) => {
            tracing::info!(
                width,
                height,
                bitrate_kbps = bitrate,
                max_fps = MAX_ENCODE_FPS,
                codec = wanted.sdp_name(),
                "video encoder created for camera"
            );
            *encoder = Some(enc);
            *last_keyframe = std::time::Instant::now();
        }
        // Left as-is on failure rather than cleared: an encoder that already works is
        // better than none, and a codec we cannot build is a negotiation fault to report,
        // not a reason to stop sending video.
        Err(e) => tracing::error!(
            codec = wanted.sdp_name(),
            "Failed to create video encoder: {e}"
        ),
    }
}

/// Decide whether this frame should be a keyframe, and tell the encoder if so.
///
/// Generic over the encoder: nothing here depends on which codec is in use, and stating
/// that as a bound rather than a concrete type is what keeps it true.
fn maybe_request_keyframe<E: VideoEncoder>(
    track_id: &str,
    encoder: &mut E,
    last_keyframe: &mut std::time::Instant,
    keyframe_requested: &Arc<std::sync::atomic::AtomicBool>,
) {
    // A receiver asking is the urgent case: until it gets a keyframe it displays a broken
    // picture and keeps asking. The timer is the backstop for a receiver that subscribes
    // without asking, since a codec's own keyframe distance can be minutes.
    let asked = keyframe_requested.swap(false, std::sync::atomic::Ordering::Relaxed);
    // A receiver that is badly out of sync sends requests faster than keyframes can help.
    // Honouring every one throws away the encoder's rate control and makes the picture
    // worse than the fault being recovered from. One keyframe in flight is the most that
    // can help.
    let recently = last_keyframe.elapsed() < MIN_KEYFRAME_GAP;
    if (asked && !recently) || last_keyframe.elapsed() >= KEYFRAME_INTERVAL {
        encoder.request_keyframe();
        *last_keyframe = std::time::Instant::now();
        tracing::info!(track_id, on_request = asked, "requested a video keyframe");
    }
}

/// Encode one captured frame and hand the packets to the connection.
///
/// Generic over the encoder rather than taking a trait object: this runs for every
/// captured frame for the length of a call, so the calls are statically dispatched and
/// inlinable, and the bound documents that the body depends on nothing but the interface.
fn encode_and_send<E: VideoEncoder>(
    key: MediaTrackKey,
    encoder: &mut E,
    frame: &elementium_media::captured_frame::CapturedFrame,
    encode_tx: &Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
) {
    // Asked of the encoder rather than of `ActiveCodec`: the encoder is what produced these
    // bytes, and it may still be the previous one for a frame or two after the SFU asks for
    // a change. The payload type and the E2EE framing both follow from this, and both are
    // wrong in ways nothing local would notice if it named a codec the bytes are not in.
    let codec = encoder.codec();
    // The compressed bytes go straight to the encoder where it can take them, which on a
    // GPU with a JPEG block means the CPU never touches a pixel of this frame. `None` says
    // it cannot, and then the frame is decoded here as it always was.
    let fast_path = frame.mjpeg().and_then(|jpeg| encoder.encode_mjpeg(jpeg));
    let outcome = if let Some(result) = fast_path {
        result
    } else {
        let Some(planar) = frame.to_planar() else {
            tracing::debug!("captured frame could not be decoded");
            return;
        };
        encoder.encode(&planar)
    };

    match outcome {
        Ok(packets) => {
            if let Ok(guard) = encode_tx.lock()
                && let Some(tx) = guard.as_ref()
            {
                for packet in packets {
                    let _ = tx.try_send(IoCommand::WriteVideo(key, packet.data, codec));
                }
            }
        }
        Err(e) => tracing::debug!("video encode error: {e}"),
    }
}

/// Which pipeline a frame belongs to.
///
/// The routing key and the IPC track id always travel together -- one addresses the m-line
/// the frame is written to, the other the preview buffer the webview reads -- so they are
/// passed as one thing rather than as two parameters that could disagree.
#[derive(Clone, Copy)]
struct PipelineId<'a> {
    key: MediaTrackKey,
    track_id: &'a str,
}

/// Encode one captured frame, keeping the encoder valid and honouring keyframe requests.
fn encode_and_send_video_frame(
    id: PipelineId<'_>,
    frame: &elementium_media::captured_frame::CapturedFrame,
    encoder: &mut Option<NegotiatedEncoder>,
    last_keyframe: &mut std::time::Instant,
    keyframe_requested: &Arc<std::sync::atomic::AtomicBool>,
    active_codec: &Arc<ActiveCodec>,
    encode_tx: &Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
) {
    let wanted = active_codec.get();
    let existed = encoder.as_ref().is_some_and(|e| e.codec() == wanted);
    ensure_encoder(
        encoder,
        frame.width(),
        frame.height(),
        last_keyframe,
        wanted,
    );

    let Some(enc) = encoder.as_mut() else {
        return;
    };
    // A freshly built encoder emits a keyframe on its own, so only ask when it is one we
    // were already using.
    if existed {
        maybe_request_keyframe(id.track_id, enc, last_keyframe, keyframe_requested);
    }
    encode_and_send(id.key, enc, frame, encode_tx);
}

/// Counters for one run of [`audio_capture_loop`], so a silent far end is diagnosable.
#[derive(Default)]
struct OutboundAudioStats {
    captured: u64,
    encoded: u64,
    sent: u64,
    skipped_not_connected: u64,
    /// Frames lost because the transport they were addressed to no longer exists.
    ///
    /// Counted apart from `dropped_channel_full` because they are opposite problems. A
    /// full channel is back-pressure: the consumer is alive and behind, and the fix is to
    /// send less or consume faster. A closed one means the peer connection went away and
    /// nothing re-attached the pipeline, so every frame from here on is wasted -- and
    /// while the two shared a counter it read as congestion, which is what hid a call
    /// going permanently silent after a mid-call renegotiation.
    dropped_channel_closed: u64,
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
        self.bytes_since_report = self
            .bytes_since_report
            .saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
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
                dropped_channel_closed = self.dropped_channel_closed,
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
    key: MediaTrackKey,
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
            match deliver(encode_tx, IoCommand::WriteAudio(key, encoded_frame)) {
                Delivery::Sent => stats.sent = stats.sent.saturating_add(1),
                Delivery::Full => {
                    stats.dropped_channel_full = stats.dropped_channel_full.saturating_add(1);
                }
                Delivery::Closed => {
                    stats.dropped_channel_closed = stats.dropped_channel_closed.saturating_add(1);
                    tracing::warn!(
                        "the peer connection this microphone was feeding has closed; \
                         audio is detached until a connection replaces it"
                    );
                }
                Delivery::Unattached => {}
            }
        }
        Err(e) => {
            stats.encode_errors = stats.encode_errors.saturating_add(1);
            tracing::debug!("Opus encode error: {e}");
        }
    }
}

/// What happened to a frame handed to a transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    Sent,
    /// The consumer is alive and behind. Back-pressure; the frame is dropped and the next
    /// one may well succeed.
    Full,
    /// The consumer is gone. Nothing will succeed again on this channel.
    Closed,
    /// Nothing was attached to send to.
    Unattached,
}

/// Hand one frame to the attached transport, letting go of it if it has gone.
///
/// The detach is the point. A closed channel never reopens -- the peer connection it
/// belonged to was torn down -- so continuing to encode into it wastes the rest of the call
/// and, while `Closed` and `Full` shared a counter, looked exactly like congestion.
/// Clearing the slot makes the state honest: the pipeline reports itself unattached, and
/// the next connection created can adopt it.
fn deliver(
    slot: &Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
    command: IoCommand,
) -> Delivery {
    let outcome = {
        let Ok(guard) = slot.lock() else {
            return Delivery::Unattached;
        };
        let Some(ref tx) = *guard else {
            return Delivery::Unattached;
        };
        match tx.try_send(command) {
            Ok(()) => Delivery::Sent,
            Err(tokio_mpsc::error::TrySendError::Full(_)) => Delivery::Full,
            Err(tokio_mpsc::error::TrySendError::Closed(_)) => Delivery::Closed,
        }
    };
    if outcome == Delivery::Closed
        && let Ok(mut guard) = slot.lock()
    {
        *guard = None;
    }
    outcome
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
    key: MediaTrackKey,
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
                encode_and_send_frame(key, &mut ctx, frame_data, encode_tx, &mut stats);

                retune_fec_if_needed(&mut encoder, loss_estimate, &mut stats);

                stats.maybe_report(sample_rate, opus_rate);
            }
        } else {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

/// Background thread: captures share audio from one `PipeWire` node, `Opus`-encodes, and
/// sends to a peer connection when `encode_tx` is connected.
///
/// The audio-source twin of [`audio_capture_loop`], not a reuse of it: the microphone goes
/// through `cpal`, which cannot address a specific `PipeWire` node (see the module docs on
/// [`elementium_media::pipewire_audio`]), so this reads from [`PipewireAudioCapture`]
/// instead. Encoding is otherwise the same shape -- mono, 20ms Opus frames -- deliberately,
/// so nothing downstream of the encoder needs to know which source produced the audio.
///
/// Kept without the microphone loop's full `OutboundAudioStats` machinery: that scaffolding
/// exists to diagnose a live human's voice sounding wrong, and share audio has had no
/// reports to diagnose yet. Frame and drop counts are still logged periodically, which is
/// enough to tell "capturing nothing" from "capturing and sending" without carrying the
/// microphone path's per-symptom counters for a symptom nobody has seen here.
fn screen_share_audio_capture_loop(
    key: MediaTrackKey,
    node_id: u32,
    source_kind: elementium_media::pipewire_audio::AudioSourceKind,
    encode_tx: &Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
    stop_rx: &std::sync::mpsc::Receiver<()>,
) {
    use elementium_media::pipewire_audio::{PipewireAudioCapture, TARGET_SAMPLE_RATE};

    let capturer = match PipewireAudioCapture::start(node_id, source_kind) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(node_id, reason = %e, "failed to start share audio capture");
            return;
        }
    };

    // The capture always negotiates TARGET_SAMPLE_RATE (see PipewireAudioCapture::start):
    // PipeWire's own converting adapter guarantees it, so there is no format to wait for the
    // way the microphone path waits on cpal's device negotiation.
    let opus_rate = TARGET_SAMPLE_RATE;
    let mut encoder = match OpusEncoder::with_config(
        opus_rate,
        OUTBOUND_CHANNELS,
        OpusEncoderConfig::default(),
    ) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(node_id, "failed to create share-audio Opus encoder: {e}");
            return;
        }
    };

    let frame_samples = usize::try_from(opus_rate)
        .unwrap_or(48_000)
        .saturating_mul(20)
        / 1000;
    let mut accumulator: Vec<f32> = Vec::with_capacity(frame_samples.saturating_mul(2));
    let mut frames_in: u64 = 0;
    let mut frames_out: u64 = 0;

    loop {
        if stop_rx.try_recv().is_ok() {
            tracing::info!(node_id, frames_in, frames_out, "share audio pipeline stopping");
            break;
        }
        if capturer.failed() {
            tracing::warn!(node_id, frames_in, frames_out, "share audio stream failed; stopping");
            break;
        }

        let Some(frame) = capturer.try_recv() else {
            std::thread::sleep(std::time::Duration::from_millis(5));
            continue;
        };
        frames_in = frames_in.saturating_add(1);

        // Folded to mono for the same reason the microphone is: Opus here is always
        // negotiated mono (OUTBOUND_CHANNELS), so a stereo source has to be reduced before
        // framing regardless of how many channels PipeWire negotiated for it.
        let mono = elementium_media::audio_capture::downmix_to_mono(&frame.data, frame.channels);
        accumulator.extend_from_slice(&mono);

        while accumulator.len() >= frame_samples {
            let frame_data: Vec<f32> = accumulator.drain(..frame_samples).collect();
            let audio_frame = AudioFrame {
                sample_rate: opus_rate,
                channels: OUTBOUND_CHANNELS,
                data: frame_data,
                timestamp_us: 0,
            };
            match encoder.encode(&audio_frame) {
                Ok(packet) => {
                    if deliver(encode_tx, IoCommand::WriteAudio(key, packet)) == Delivery::Sent {
                        frames_out = frames_out.saturating_add(1);
                    }
                }
                Err(e) => tracing::debug!(node_id, "share audio Opus encode error: {e}"),
            }
        }
    }

    let dropped = capturer.dropped();
    if dropped > 0 {
        tracing::info!(node_id, dropped, "share audio buffers dropped over the pipeline's life");
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
            .checked_div(
                MIN_ENCODE_INTERVAL
                    .as_nanos()
                    .try_into()
                    .unwrap_or(u64::MAX),
            )
            .unwrap_or(0);
        assert_eq!(per_second, MAX_ENCODE_FPS);
    }
}

#[cfg(test)]
mod requested_fps_tests {
    use super::{MAX_ENCODE_FPS_U32, requested_fps};

    /// A call asks for 30 and streaming asks for 60; both must reach the camera intact.
    /// Capping capture at the call rate would silently halve a stream.
    #[test]
    fn common_rates_pass_through() {
        assert_eq!(requested_fps(30.0), 30);
        assert_eq!(requested_fps(60.0), 60);
        assert_eq!(requested_fps(120.0), 120);
        assert_eq!(requested_fps(24.0), 24);
    }

    /// A rate between the ones a camera offers picks the nearest, not the floor: asking
    /// for 59 and getting 30 would halve the stream the caller asked for.
    #[test]
    fn an_unusual_rate_picks_the_nearest_offered() {
        assert_eq!(requested_fps(59.0), 60);
        assert_eq!(requested_fps(29.0), 30);
        // Equidistant between 60 and 120: the tie breaks downwards, because exceeding
        // what was asked for spends CPU and bitrate nobody requested.
        assert_eq!(requested_fps(90.0), 60);
    }

    /// Nonsense must not reach the camera. Zero would ask for a source that never
    /// delivers, and an absurd rate would have us decoding frames nothing consumes.
    #[test]
    fn nonsense_falls_back_to_the_default() {
        assert_eq!(requested_fps(f64::NAN), MAX_ENCODE_FPS_U32);
        assert_eq!(requested_fps(f64::INFINITY), MAX_ENCODE_FPS_U32);
        assert_eq!(requested_fps(0.0), 1, "clamped, not zero");
        assert_eq!(requested_fps(-5.0), 1);
        assert_eq!(
            requested_fps(100_000.0),
            120,
            "clamped to the highest offered"
        );
    }
}

#[cfg(test)]
mod active_codec_tests {
    use super::{ActiveCodec, DEFAULT_VIDEO_CODEC};
    use elementium_codec::VideoCodec;

    /// The codec must be changeable while a call is running.
    ///
    /// The case this exists for: a room where everyone decodes AV1 gains a participant who
    /// cannot. The SFU does not transcode, so the publisher has to move -- and it has to
    /// move without restarting the track, because renegotiating drops video for everyone
    /// to accommodate one late arrival.
    #[test]
    fn the_codec_can_change_during_a_call() {
        let active = ActiveCodec::new(VideoCodec::Av1);
        assert_eq!(active.get(), VideoCodec::Av1);

        active.set(VideoCodec::Vp8);
        assert_eq!(active.get(), VideoCodec::Vp8, "regression must take effect");

        active.set(VideoCodec::H264);
        assert_eq!(active.get(), VideoCodec::H264, "and be able to move again");
    }

    /// Every codec must survive the round trip through the shared cell. A mapping that
    /// loses one would encode with a codec nobody agreed to, which the far end receives as
    /// undecodable video.
    #[test]
    fn every_codec_round_trips() {
        for &codec in VideoCodec::all() {
            let active = ActiveCodec::new(codec);
            assert_eq!(active.get(), codec, "{codec:?} did not survive");
        }
    }

    /// An unrecognised value must decode to the codec every peer speaks. A wrong codec is
    /// worse than a slow one: it produces video nobody can decode.
    #[test]
    fn the_default_is_the_universally_supported_codec() {
        assert_eq!(DEFAULT_VIDEO_CODEC, VideoCodec::Vp8);
        assert_eq!(ActiveCodec::new(DEFAULT_VIDEO_CODEC).get(), VideoCodec::Vp8);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod delivery_tests {
    use super::{Delivery, deliver};
    use elementium_types::{MediaTrackKey, PlaintextMedia};
    use elementium_webrtc::engine::IoCommand;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc as tokio_mpsc;

    fn frame() -> IoCommand {
        IoCommand::WriteAudio(
            MediaTrackKey::microphone(),
            PlaintextMedia::from_encoder(vec![1, 2, 3]),
        )
    }

    /// The ordinary case: a live consumer takes the frame.
    #[test]
    fn a_live_transport_takes_the_frame() {
        let (tx, _rx) = tokio_mpsc::channel(4);
        let slot = Arc::new(Mutex::new(Some(tx)));
        assert_eq!(deliver(&slot, frame()), Delivery::Sent);
        assert!(
            slot.lock().expect("lock").is_some(),
            "a working transport must stay attached"
        );
    }

    /// Back-pressure is not disconnection. A full channel means the consumer is alive and
    /// behind, so the attachment must survive -- dropping it here would end a call for a
    /// moment of congestion.
    #[test]
    fn a_full_transport_is_kept() {
        let (tx, _rx) = tokio_mpsc::channel(1);
        let slot = Arc::new(Mutex::new(Some(tx)));
        assert_eq!(deliver(&slot, frame()), Delivery::Sent);
        assert_eq!(deliver(&slot, frame()), Delivery::Full);
        assert!(
            slot.lock().expect("lock").is_some(),
            "congestion must not detach a live transport"
        );
    }

    /// The case this exists for. A peer connection torn down mid-call leaves a sender
    /// whose receiver is gone; every later frame is wasted, and while this was counted as
    /// congestion it read as a busy call rather than a dead one.
    #[test]
    fn a_closed_transport_is_let_go_of() {
        let (tx, rx) = tokio_mpsc::channel(4);
        let slot = Arc::new(Mutex::new(Some(tx)));
        drop(rx);

        assert_eq!(deliver(&slot, frame()), Delivery::Closed);
        assert!(
            slot.lock().expect("lock").is_none(),
            "a closed transport must be released, so the pipeline reports itself unattached \
             and the next connection can adopt it"
        );
        // And having let go, it says so rather than reporting a closed channel forever.
        assert_eq!(deliver(&slot, frame()), Delivery::Unattached);
    }

    /// Nothing attached is its own outcome, distinct from a failure to send.
    #[test]
    fn an_empty_slot_reports_itself() {
        let slot: Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>> = Arc::new(Mutex::new(None));
        assert_eq!(deliver(&slot, frame()), Delivery::Unattached);
    }
}
