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

use super::webrtc::WebRtcState;
use super::{IpcErr, LockExt};
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
        /// Fires once this pipeline's capturer has actually dropped and released the
        /// device, so a replacement pipeline can wait for it instead of racing it.
        ///
        /// Only the microphone path (`start_audio_pipeline`) populates this: it is the only
        /// audio pipeline that owns a `cpal::Stream`, which is the thing that needs to be
        /// gone before the next one opens the same device. Screen-share audio reads from
        /// `PipeWire`, which does not have this contention, so it leaves this `None`.
        release_rx: Option<std::sync::mpsc::Receiver<()>>,
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
        /// The bitrate last requested through `RTCRtpSender.setParameters`, in kbps.
        ///
        /// A level, like `keyframe_requested` and `active_codec` above: `setParameters` can
        /// be called any number of times over a call's life, and the encoder thread only
        /// ever needs the latest value. `0` means "nothing requested yet" -- the command
        /// that writes this clamps every real request to at least `MIN_BITRATE_KBPS`, so `0`
        /// can never collide with a legitimate one.
        bitrate_override: Arc<std::sync::atomic::AtomicU32>,
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
    /// Set while the user has this track muted.
    ///
    /// Muting has to stop the *media*, not just the icon. Element Call mutes by setting
    /// `enabled = false` on the `MediaStreamTrack` it was handed, and that object is a
    /// local preview -- the frames on the wire come from this pipeline, which knew nothing
    /// about it. So a muted microphone kept being published: the user saw a muted icon and
    /// everyone else kept hearing them. Checked in the capture loop rather than at the
    /// channel, so a muted track also stops spending CPU on encoding nobody will receive.
    pub muted: Arc<std::sync::atomic::AtomicBool>,
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
                ..
            } => Some((keyframe_requested, active_codec)),
            PipelineExtras::Audio { .. } => None,
        }
    }

    /// The measured outbound loss, if this is an audio pipeline.
    #[must_use]
    pub const fn loss_estimate(&self) -> Option<&Arc<NetworkLossEstimate>> {
        match &self.extras {
            PipelineExtras::Audio { loss_estimate, .. } => Some(loss_estimate),
            PipelineExtras::Video { .. } => None,
        }
    }

    /// The live `setParameters` bitrate override, if this is a video pipeline.
    #[must_use]
    pub const fn bitrate_override(&self) -> Option<&Arc<std::sync::atomic::AtomicU32>> {
        match &self.extras {
            PipelineExtras::Video { bitrate_override, .. } => Some(bitrate_override),
            PipelineExtras::Audio { .. } => None,
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
    /// The correlation id of the call currently connected, if any.
    ///
    /// Capture already logged under a correlation id, but a *fresh* one minted per
    /// `get_user_media`, which is worse than none: two ids for one call look like two
    /// unrelated flows, and joining a camera's frames to the session that published them
    /// was impossible in a log. FR-002 asks for one id across a user flow, and the flow
    /// starts at the room, not at the device.
    ///
    /// `None` outside a call, which is a real state rather than an error: device
    /// enumeration and a preview both run before anyone joins anything, and they get their
    /// own id.
    pub session_correlation: Mutex<Option<CorrelationId>>,
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
    let bitrate_override = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let bitrate_override_clone = Arc::clone(&bitrate_override);
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let muted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let muted_clone = Arc::clone(&muted);

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
    // A span of this pipeline's own, not merely whatever was current at spawn.
    //
    // Everything `elementium-media` logs -- format negotiation, dropped frames, decode
    // cost, a node disappearing -- is emitted on this thread and inherits whatever span it
    // runs under. Inheriting the caller's meant inheriting nothing, because a share is
    // started from a Tauri command that carries no call context, so capture events could
    // not be tied to the track they came from. FR-002 asks for exactly that link, and this
    // is the cheapest place to make it: one span here labels every event underneath it,
    // including the ones in crates that know nothing about tracks.
    // The call's correlation id is read here, in synchronous code, rather than entered as
    // a span guard in the async command above: a guard held across an `.await` is attached
    // to whichever thread resumes the future, so it can end up decorating unrelated work.
    // This span belongs to a thread that never awaits, which is exactly where it is safe.
    let call_id = media_state
        .session_correlation
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .unwrap_or_default();
    let span = tracing::info_span!(
        parent: tracing::Span::current(),
        "capture",
        correlation_id = %call_id,
        track_id = %track_id,
        key = %key
    );
    let controls = VideoPipelineControls {
        keyframe_requested: keyframe_requested_clone,
        active_codec: active_codec_clone,
        bitrate_override: bitrate_override_clone,
    };
    std::thread::spawn(move || {
        let _guard = span.enter();
        video_pipeline_loop(
            key,
            &muted_clone,
            &tid,
            &capture_source,
            &frames,
            &encode_tx_clone,
            &controls,
            &stop_rx,
        );
    });

    media_state
        .pipelines
        .lock_str("get_display_media")?
        .insert(
            key,
            PipelineHandle {
                key,
                track_id: track_id.clone(),
                stop_tx,
                encode_tx,
                muted,
                extras: PipelineExtras::Video {
                    keyframe_requested,
                    active_codec,
                    bitrate_override,
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
    let muted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let muted_clone = Arc::clone(&muted);

    let span = tracing::Span::current();
    std::thread::spawn(move || {
        let _guard = span.enter();
        screen_share_audio_capture_loop(
            key,
            node_id,
            source_kind,
            &encode_tx_clone,
            &muted_clone,
            &stop_rx,
        );
    });

    media_state
        .pipelines
        .lock_str("get_display_media")?
        .insert(
            key,
            PipelineHandle {
                key,
                track_id: track_id.clone(),
                stop_tx,
                encode_tx,
                muted,
                // No `cpal::Stream` to hand off here -- see the field's doc comment.
                extras: PipelineExtras::Audio {
                    loss_estimate,
                    release_rx: None,
                },
            },
        );

    if let Ok(mut tracks) = media_state.active_tracks.lock() {
        tracks.push(TrackId(track_id.clone()));
    }

    Ok(track_id)
}

/// Mute or unmute a capture pipeline, stopping the media rather than only the icon.
///
/// Element Call mutes by setting `enabled = false` on the `MediaStreamTrack` it was handed.
/// That object is a local preview: the frames on the wire come from a capture pipeline in
/// Rust which never saw it. So muting changed nothing anyone else could hear or see — the
/// user got a muted icon and kept broadcasting. The shim now forwards the change here.
///
/// Signalling the SFU is a separate call (`livekit_set_track_muted`), because a mute has to
/// stop the media whether or not a room is connected, and the media is the part that
/// matters if only one of the two can happen.
///
/// # Errors
///
/// Returns an error if the kind/source pair is not one we publish, or if there is no
/// pipeline for it — muting a track that is not running cannot be reported as done.
// Tauri's IPC gives a command owned arguments and an owned `State` handle; taking them by
// reference is not an option the macro offers.
#[allow(clippy::needless_pass_by_value)]
#[command]
pub fn set_capture_muted(
    media_state: State<'_, MediaState>,
    kind: String,
    source: String,
    muted: bool,
) -> Result<(), String> {
    let key = super::livekit::track_key("set_capture_muted", &kind, &source)?;
    // The flag is taken out from under the guard rather than used through it, so the lock
    // is not held while anything else happens -- this runs on the IPC thread, and the
    // capture loops take the same map.
    let flag = {
        let pipelines = media_state.pipelines.lock_str("set_capture_muted")?;
        pipelines.get(&key).map(|handle| Arc::clone(&handle.muted))
    };
    let Some(flag) = flag else {
        // Expected under normal use: the page can call this before a pipeline has started
        // (joining muted) or after it stopped, and both race the mute request harmlessly --
        // see the "no video pipeline" case below for the same reasoning.
        tracing::warn!(track = %key, "mute requested for a track with no running capture pipeline");
        return Err(format!("no capture pipeline is running for {key}"));
    };
    flag.store(muted, std::sync::atomic::Ordering::Relaxed);
    tracing::info!(track = %key, muted, "capture mute state changed");
    Ok(())
}

/// Apply an `RTCRtpSender.setParameters` bitrate request to a running video pipeline.
///
/// Before this existed, `setParameters` resolved successfully and changed nothing: nothing
/// stored what it was called with, and `getParameters` kept returning the encodings frozen
/// at `addTransceiver` time. Every bandwidth adaptation `livekit-client` believed it made --
/// 15 call sites in the shipped bundle -- was silently discarded, which is why video that
/// degrades under a bad link never recovers: the client keeps asking for less and the
/// encoder never hears it.
///
/// `max_bitrates_bps` is one entry per encoding the caller passed, `None` where an encoding
/// had no `maxBitrate`. Simulcast is not implemented, so more than one entry collapses to a
/// single aggregate cap ([`requested_bitrate_kbps`]) rather than driving separate layers;
/// logged once here so that is visible rather than silently approximated.
///
/// Returns the kbps actually applied, or `None` if no encoding carried a `maxBitrate` -- in
/// which case, per policy, nothing changed and the pipeline keeps its previous bitrate.
///
/// # Errors
///
/// Returns an error if the kind/source pair is not one we publish, or if there is no video
/// pipeline running for it.
#[allow(clippy::needless_pass_by_value)]
#[command]
pub fn set_video_bitrate(
    media_state: State<'_, MediaState>,
    kind: String,
    source: String,
    max_bitrates_bps: Vec<Option<u32>>,
) -> Result<Option<u32>, String> {
    let key = super::livekit::track_key("set_video_bitrate", &kind, &source)?;

    let Some(requested_kbps) = requested_bitrate_kbps(&max_bitrates_bps) else {
        tracing::info!(
            track = %key,
            encodings = max_bitrates_bps.len(),
            "setParameters carried no maxBitrate on any encoding; leaving the encoder as is"
        );
        return Ok(None);
    };

    if max_bitrates_bps.len() > 1 {
        tracing::info!(
            track = %key,
            encodings = max_bitrates_bps.len(),
            "setParameters supplied multiple encodings; simulcast is not implemented here, \
             applying only an aggregate bitrate cap"
        );
    }

    let (kbps, clamped) = clamp_bitrate_kbps(requested_kbps);
    if clamped {
        tracing::warn!(
            track = %key,
            requested_kbps,
            applied_kbps = kbps,
            min_kbps = MIN_BITRATE_KBPS,
            max_kbps = MAX_BITRATE_KBPS,
            "setParameters requested a bitrate outside the sane range; clamped"
        );
    }

    // Same lock-then-clone-then-release shape as `set_capture_muted`: the flag is taken out
    // from under the guard so the pipeline map is not held while anything else happens, and
    // this runs on the IPC thread while the capture loop takes the same map.
    let flag = {
        let pipelines = media_state.pipelines.lock_str("set_video_bitrate")?;
        pipelines.get(&key).and_then(PipelineHandle::bitrate_override).cloned()
    };
    let Some(flag) = flag else {
        // Expected, not a fault: livekit-client can call setParameters on a sender before
        // the corresponding capture pipeline has (re)started, e.g. immediately after a
        // device change.
        tracing::warn!(track = %key, "setParameters requested for a track with no running video pipeline");
        return Err(format!("no video pipeline is running for {key}"));
    };
    flag.store(kbps, std::sync::atomic::Ordering::Relaxed);
    tracing::info!(track = %key, kbps, "video bitrate changed via setParameters");
    Ok(Some(kbps))
}

/// The index inside an `audio-input-{n}` device id, which is cpal's own enumeration order.
///
/// `None` for anything else, in which case the default device is used — the same rule the
/// camera path applies to ids it cannot resolve.
fn microphone_index(device_id: &str) -> Option<usize> {
    device_id.strip_prefix("audio-input-")?.parse().ok()
}


/// The cameras the user can choose from, named so that capture can find them again.
///
/// Enumerated from `PipeWire` first, because that is what capture actually opens. They used
/// to disagree: this listed nokhwa's devices as `video-input-{index}` while capture walked
/// `PipeWire`'s own node list in its own order, so the id a user picked could not be
/// resolved back to a node — which is why choosing a camera in settings did nothing at all
/// and you got whichever one enumeration happened to reach first.
///
/// The same disagreement, between an enumeration and the thing that starts capture, was the
/// screen-share bug recorded as R6 in `specs/008-screen-share-capture/research.md`. It is
/// worth naming twice.
///
/// nokhwa remains the fallback for a machine with no `PipeWire`, where capture falls back
/// to V4L2 in the same order, so the two still agree.
fn video_input_devices() -> Vec<MediaDevice> {
    match elementium_media::pipewire_nodes::list_video_sources() {
        Ok(sources) if !sources.is_empty() => sources
            .into_iter()
            .map(|s| MediaDevice {
                id: format!("video-input-pw-{}", s.node_id),
                label: s.description,
                kind: elementium_types::MediaDeviceKind::VideoInput,
            })
            .collect(),
        Ok(_) | Err(_) => nokhwa::query(nokhwa::utils::ApiBackend::Auto)
            .map(|cameras| {
                cameras
                    .iter()
                    .enumerate()
                    .map(|(i, cam)| MediaDevice {
                        id: format!("video-input-{i}"),
                        label: cam.human_name(),
                        kind: elementium_types::MediaDeviceKind::VideoInput,
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// The `PipeWire` node id inside a device id handed out by `enumerate_devices`.
///
/// `None` for the nokhwa fallback ids (`video-input-3`), which name an index into a
/// different enumeration and cannot be resolved to a node — the capture path then keeps its
/// existing behaviour of taking the first source that works.
fn camera_node_id(device_id: Option<&str>) -> Option<u32> {
    let requested = device_id?;
    if let Some(node) = requested.strip_prefix("video-input-pw-").and_then(|n| n.parse().ok()) {
        return Some(node);
    }
    // Every other id shape reaches here, and the capture path then takes the first camera
    // that opens -- which for a `video-input-N` id from the nokhwa fallback means the user
    // picked a camera and silently got whichever one came first. Saying so costs one line
    // and turns "the picker does nothing on this machine" from a mystery into a log entry.
    tracing::warn!(
        device_id = %requested,
        reason = "unresolvable_camera_id",
        "cannot resolve this camera id to a PipeWire node; capture will use the first \
         camera that opens, which may not be the one that was chosen"
    );
    None
}

#[command]
pub async fn enumerate_devices() -> Result<Vec<MediaDevice>, String> {
    tracing::info!("Enumerating media devices");

    let mut devices = device_enumeration::enumerate_audio_devices();
    devices.extend(video_input_devices());
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
    /// The old pipeline's device-release acknowledgement, if it was a microphone pipeline
    /// (see [`PipelineExtras::Audio::release_rx`]). The caller waits on this before opening
    /// the same device again, rather than racing the old capturer's drop.
    release_rx: Option<std::sync::mpsc::Receiver<()>>,
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

/// The two channels `audio_capture_loop` needs to hand a pipeline over cleanly, bundled so
/// the function stays within the workspace's argument-count lint: a stop signal in, and a
/// release acknowledgement out once the capturer backing it has actually dropped.
struct AudioCaptureHandoff {
    stop_rx: std::sync::mpsc::Receiver<()>,
    release_tx: std::sync::mpsc::Sender<()>,
}

/// The longest a new microphone pipeline waits for the previous one to confirm its capturer
/// has dropped before opening the device anyway.
///
/// An upper bound only, not the expected wait: the acknowledgement normally arrives as soon
/// as the old capture thread next wakes from its 5ms sleep and drops its `cpal::Stream`, well
/// under this. Sized the same order of magnitude as the camera path's fixed 500ms sleep
/// (empirically enough for the OS to let a device go) so a genuinely wedged old thread does
/// not stall the new one indefinitely -- a call with the wrong timing beats a call with no
/// microphone at all, but the miss is logged rather than silent.
const AUDIO_HANDOVER_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(750);

/// Wait for a stopped microphone pipeline's capturer to actually release the device,
/// bounded so a wedged shutdown cannot hang the caller forever.
///
/// Pure and independent of [`MediaState`] so the handover ordering itself is testable
/// without spinning up real pipelines or real audio hardware: see the tests below.
///
/// Returns whether the acknowledgement arrived before `timeout` elapsed. `false` means the
/// caller should proceed anyway -- but the miss belongs in the log, since it means the two
/// capture streams may briefly have overlapped, which is the fault this function exists to
/// prevent.
fn wait_for_capturer_release(
    release_rx: &std::sync::mpsc::Receiver<()>,
    timeout: std::time::Duration,
) -> bool {
    release_rx.recv_timeout(timeout).is_ok()
}

/// Start (or restart) the microphone pipeline, returning the new track's id.
///
/// Replaces whatever was running and takes over its connection, so a mid-call restart
/// keeps sending -- see [`stop_pipeline_inheriting_connection`].
///
/// Waits for the previous capturer to confirm it released the device before opening it
/// again (see [`wait_for_capturer_release`]). Before this, the new capture thread was
/// spawned the instant `stop_tx` was sent, with no wait for the old thread to actually drop
/// its `cpal::Stream` -- so two streams could briefly hold the same physical input, and the
/// loser (observed to be the *new* stream, not the old one) got called back on schedule with
/// nothing in the buffer: `input_peak` and `channel_peaks` at exactly zero from the first
/// frame, while the encoder kept running and `sent` climbed. That is a live pipeline
/// publishing silence, with nothing in the log to say why.
fn start_audio_pipeline(
    media_state: &MediaState,
    device_index: Option<usize>,
    auto_gain: bool,
) -> TrackId {
    let key = MediaTrackKey::microphone();
    let track_id = TrackId(format!("audio-{}", generate_track_id()));
    tracing::info!(track_id = %track_id, "Starting audio capture");

    let previous =
        stop_pipeline_inheriting_connection(&media_state.pipelines, MediaTrackKey::microphone());
    if let Some(release_rx) = previous.release_rx {
        let acked = wait_for_capturer_release(&release_rx, AUDIO_HANDOVER_TIMEOUT);
        if !acked {
            tracing::warn!(
                track_id = %track_id,
                timeout_ms = AUDIO_HANDOVER_TIMEOUT.as_millis(),
                "previous microphone capturer did not confirm release before the handover \
                 timeout; opening the new device anyway"
            );
        }
    }
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
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let muted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let muted_clone = Arc::clone(&muted);
    let handoff = AudioCaptureHandoff {
        stop_rx,
        release_tx,
    };

    // Inherits the call's correlation span so every event the thread emits carries the
    // same correlation_id.
    let audio_span = tracing::Span::current();
    std::thread::spawn(move || {
        let _guard = audio_span.enter();
        audio_capture_loop(
            key,
            device_index,
            auto_gain,
            &encode_tx_clone,
            &muted_clone,
            &handoff,
            &loss_estimate_clone,
        );
    });

    if let Ok(mut pipelines) = media_state.pipelines.lock() {
        pipelines.insert(
            MediaTrackKey::microphone(),
            PipelineHandle {
                key: MediaTrackKey::microphone(),
                track_id: track_id.0.clone(),
                stop_tx,
                encode_tx,
                muted,
                extras: PipelineExtras::Audio {
                    loss_estimate,
                    release_rx: Some(release_rx),
                },
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
            release_rx: None,
        };
    };
    let Some(old) = guard.remove(&key) else {
        return StoppedPipeline {
            existed: false,
            connection: None,
            release_rx: None,
        };
    };
    let _ = old.stop_tx.send(());
    let connection = old.encode_tx.lock().ok().and_then(|c| c.clone());
    let release_rx = match old.extras {
        PipelineExtras::Audio { release_rx, .. } => release_rx,
        PipelineExtras::Video { .. } => None,
    };
    StoppedPipeline {
        existed: true,
        connection,
        release_rx,
    }
}

/// Stop any camera pipeline already running and inherit whatever it was feeding.
///
/// Returns whether one existed -- the caller waits for the device to release if so -- and
/// the peer connection to adopt.
///
/// The inheritance matters for exactly the reason it does on the audio path: a camera
/// restarted mid-call (device change, resolution change, track replacement) would otherwise
/// sit disconnected until the next renegotiation, and the far end would see the video
/// freeze with nothing in the log to explain it.
fn take_over_camera_pipeline(
    media_state: &MediaState,
    track_id: &TrackId,
) -> (bool, Option<tokio_mpsc::Sender<IoCommand>>) {
    let previous =
        stop_pipeline_inheriting_connection(&media_state.pipelines, MediaTrackKey::camera());
    let had_previous = previous.existed;
    let inherited_connection = connection_for_new_pipeline(previous.connection, media_state);
    if inherited_connection.is_some() {
        tracing::info!(
            track_id = %track_id,
            "Camera restarted mid-call; inheriting the existing peer connection"
        );
    }
    (had_previous, inherited_connection)
}

/// The microphone the caller asked for, resolved the same way the picker numbered it.
/// Whether the caller asked for automatic gain control.
///
/// Defaulted on when unspecified, which is what a browser does and what every caller in
/// this stack asks for anyway. Honouring it at all is new: the flag was accepted and
/// ignored, so a quiet microphone was transmitted quiet and Opus encoded near-silence.
fn wants_auto_gain(constraints: &MediaConstraints) -> bool {
    constraints
        .audio
        .as_ref()
        .and_then(|a| a.auto_gain_control)
        .unwrap_or(true)
}

fn chosen_microphone(constraints: &MediaConstraints) -> Option<usize> {
    constraints
        .audio
        .as_ref()
        .and_then(|a| a.device_id.as_deref())
        .and_then(microphone_index)
}

/// Start a camera pipeline for one `getUserMedia` video request, returning its track id.
///
/// Split out of `get_user_media`, which handles both the audio and video halves of one
/// request in a single function -- camera startup was most of what made it too long to take
/// in as one piece, the same reasoning `start_audio_pipeline` already applies to the audio
/// half.
///
/// # Errors
///
/// Returns a description if the shared video frame buffer cannot be reached.
fn start_camera_pipeline(
    webrtc_state: &WebRtcState,
    media_state: &MediaState,
    video_constraints: &elementium_types::VideoConstraints,
) -> Result<TrackId, String> {
    let track_id = TrackId(format!("video-{}", generate_track_id()));
    tracing::info!(track_id = %track_id, "Starting video capture");

    // Get the shared video frame buffer from the WebRTC engine
    let video_frames = {
        let engine = webrtc_state.0.lock_str("get_user_media")?;
        engine.video_frames.clone()
    };

    let (had_previous, inherited_connection) = take_over_camera_pipeline(media_state, &track_id);

    let req_node_id = camera_node_id(video_constraints.device_id.as_deref());
    let req_width = video_constraints.width;
    let req_height = video_constraints.height;
    // Honour the caller's frame rate: a call wants 30, streaming wants 60 or more.
    // Asking for what will be consumed means the surplus is never decoded.
    let req_fps = video_constraints
        .frame_rate
        .map_or_else(max_encode_fps_u32, requested_fps);

    let encode_tx: Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>> =
        Arc::new(Mutex::new(inherited_connection));
    let encode_tx_clone = encode_tx.clone();
    let keyframe_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let keyframe_requested_clone = keyframe_requested.clone();
    let active_codec = Arc::new(ActiveCodec::new(DEFAULT_VIDEO_CODEC));
    let active_codec_clone = active_codec.clone();
    let bitrate_override = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let bitrate_override_clone = Arc::clone(&bitrate_override);
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let muted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let muted_clone = Arc::clone(&muted);
    let tid = track_id.0.clone();

    // Start the camera pipeline on a background thread, inheriting the
    // call's correlation span so every event it emits carries the same
    // correlation_id.
    // If we just stopped a previous pipeline, delay to let the V4L2
    // device release (avoids EBUSY on Linux).
    // Labelled the same way a share's pipeline is, and for the same reason: the camera
    // path's capture events come from `elementium-media`, which has no idea which
    // track it is feeding.
    let camera_span = tracing::info_span!(
        parent: tracing::Span::current(),
        "capture",
        track_id = %track_id,
        key = %MediaTrackKey::camera()
    );
    let controls = VideoPipelineControls {
        keyframe_requested: keyframe_requested_clone,
        active_codec: active_codec_clone,
        bitrate_override: bitrate_override_clone,
    };
    std::thread::spawn(move || {
        let _guard = camera_span.enter();
        if had_previous {
            tracing::info!("Waiting for previous camera to release device...");
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        video_pipeline_loop(
            MediaTrackKey::camera(),
            &muted_clone,
            &tid,
            &VideoCaptureSource::Camera {
                node_id: req_node_id,
                width: req_width,
                height: req_height,
                fps: req_fps,
            },
            &video_frames,
            &encode_tx_clone,
            &controls,
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
                muted,
                extras: PipelineExtras::Video {
                    keyframe_requested,
                    active_codec,
                    bitrate_override,
                },
            },
        );
    }

    if let Ok(mut tracks) = media_state.active_tracks.lock() {
        tracks.push(track_id.clone());
    }

    Ok(track_id)
}

#[command]
pub async fn get_user_media(
    webrtc_state: State<'_, WebRtcState>,
    media_state: State<'_, MediaState>,
    constraints: MediaConstraints,
) -> Result<Vec<TrackId>, String> {
    // The connected call's id when there is one, a fresh id when there is not.
    //
    // Minting one unconditionally, as this used to, gave a call two correlation ids: the
    // session's and this one. Two ids for one flow is worse than none, because they look
    // like unrelated activity and nothing hints that they belong together.
    let call_id = media_state
        .session_correlation
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .unwrap_or_default();
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
        track_ids.push(start_audio_pipeline(
            &media_state,
            chosen_microphone(&constraints),
            wants_auto_gain(&constraints),
        ));
    }

    if let Some(ref video_constraints) = constraints.video {
        track_ids.push(start_camera_pipeline(&webrtc_state, &media_state, video_constraints)?);
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

/// Said once per process, so the warning about writing the camera to disk is not repeated
/// every frame and is not skipped either.
static DUMP_ANNOUNCED: std::sync::Once = std::sync::Once::new();

/// How many preview frames have been written this session.
static DUMPS_WRITTEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The most preview frames one session will write.
///
/// Twenty frames spread over twenty seconds is more than enough to see a tear or a colour
/// swap, and bounds both the disk a forgotten sentinel can fill and how much of the user's
/// camera ends up on it.
const DUMP_LIMIT: u32 = 20;

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
    // Said once, loudly, and said at all.
    //
    // The sentinel is a file, which is what makes it usable mid-session and also what makes
    // it outlive the session that created one: it was found still enabled days later,
    // quietly writing pictures of the user to a world-readable directory, with only an INFO
    // line per dump to say so. A diagnostic that records the camera has to announce itself
    // in terms that make sense to whoever finds the log, and say how to stop it.
    DUMP_ANNOUNCED.call_once(|| {
        tracing::warn!(
            sentinel = DUMP_PREVIEW_SENTINEL,
            directory = "/tmp",
            "camera frames are being written to disk as raw images, because preview dumping \
             is switched on; delete the sentinel file to stop it"
        );
    });

    // Bounded, because a sentinel nobody deletes otherwise fills the disk a megabyte at a
    // time and leaves an hour of the user's camera lying in /tmp. Twenty frames spread over
    // twenty seconds is more than enough to see a tear or a colour swap.
    let dumped = DUMPS_WRITTEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if dumped >= DUMP_LIMIT {
        if dumped == DUMP_LIMIT {
            tracing::warn!(
                limit = DUMP_LIMIT,
                "preview dumping has written its limit and is stopping; restart the app to \
                 collect more"
            );
        }
        return;
    }

    let path = format!("/tmp/elementium_preview_{frame_count}_{width}x{height}.rgba");
    match write_private(&path, rgba) {
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

/// Write a file only its owner can read.
///
/// These are pictures of whoever is in front of the camera, in a directory every account on
/// the machine can list. The default mode is whatever the umask allows, which on a normal
/// desktop is world-readable.
fn write_private(path: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
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

/// How long a keyframe request may go unanswered before it is a fault worth a log line
/// rather than ordinary scheduling jitter.
///
/// Set above [`KEYFRAME_INTERVAL`] on purpose: a healthy encoder emits a keyframe of its
/// own accord at least that often, so if this much time passes with a request outstanding
/// and still nothing decodable has left the encoder, the periodic backstop did not save it
/// either. That is no longer "the request hasn't been serviced yet" -- it is the shape of
/// the incident this exists for, where the SFU sent 27 PLIs over the length of a call and
/// never got a keyframe back while every counter on the sending side read healthy.
const KEYFRAME_ANSWER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether a keyframe requested from the encoder has actually left it.
///
/// The request itself was already visible in the logs: `Receiver requested a keyframe` in
/// `elementium-webrtc`, once per RTCP PLI. What was missing was the other half of that
/// story -- an encoder can be asked and simply never answer, and every existing counter
/// (packets sent, loss, RTT) stays green throughout, because none of them look at whether
/// an encoded frame was decodable on its own. `is_keyframe` on the encoder's own output
/// (`EncodedFrame::is_keyframe`) is the only place that is known, so this watches it rather
/// than inferring health from anything downstream.
///
/// One episode is one continuous stretch of "asked and not yet answered". It is tracked
/// rather than logged eagerly because a stuck receiver resends its PLI several times a
/// second: logging each arrival would reproduce the exact noise problem the per-request log
/// already has, just one level deeper in the pipeline. Instead this accumulates a count and
/// speaks once when [`KEYFRAME_ANSWER_TIMEOUT`] proves the episode is a real fault, then
/// stays quiet until it either resolves or is asked again.
#[derive(Default)]
struct KeyframeAnswerWatch {
    /// When the current run of unanswered requests began. `None` means the last request
    /// (if any) was already answered, so there is nothing outstanding to time out.
    pending_since: Option<std::time::Instant>,
    /// Requests folded into the current episode, including the one that opened it.
    unanswered_requests: u32,
    /// Set once this episode has already logged, so a stuck encoder gets one line, not one
    /// every frame for as long as it stays stuck.
    warned: bool,
}

impl KeyframeAnswerWatch {
    /// Record that a receiver's request was just forwarded to the encoder.
    fn requested(&mut self) {
        if self.pending_since.is_none() {
            self.pending_since = Some(std::time::Instant::now());
            self.unanswered_requests = 1;
            self.warned = false;
        } else {
            self.unanswered_requests = self.unanswered_requests.saturating_add(1);
        }
    }

    /// Record that the encoder actually produced a keyframe, closing out any open episode.
    ///
    /// Closes on any keyframe, not just ones that followed a request: the periodic timer in
    /// `maybe_request_keyframe` proves the encoder is answerable just as well as a request
    /// does, and a request that arrived just before a scheduled keyframe was going to fire
    /// anyway is not a fault.
    const fn observed_keyframe(&mut self) {
        self.pending_since = None;
        self.unanswered_requests = 0;
        self.warned = false;
    }

    /// Speak once if the open episode has run past [`KEYFRAME_ANSWER_TIMEOUT`].
    /// Whether this episode is now due a warning, given how long it has been open.
    ///
    /// Split from the logging so the decision can be tested at an arbitrary age. The
    /// alternative -- sleeping past a five-second constant in a unit test -- buys the same
    /// assurance at the cost of a slow test, and slow tests get marked ignored and then
    /// stop being run at all.
    const fn is_due(&self, open_for: std::time::Duration) -> bool {
        self.pending_since.is_some()
            && !self.warned
            && open_for.as_millis() >= KEYFRAME_ANSWER_TIMEOUT.as_millis()
    }

    fn check_timeout(&mut self, track_id: &str) {
        let Some(since) = self.pending_since else {
            return;
        };
        if !self.is_due(since.elapsed()) {
            return;
        }
        self.warned = true;
        tracing::warn!(
            track_id,
            unanswered_requests = self.unanswered_requests,
            waited_secs = since.elapsed().as_secs_f64(),
            "keyframe requests are arriving but no keyframe has left the encoder; the far \
             end is decoding nothing and will keep asking until this resolves"
        );
    }
}

/// Timing state for one encoder's keyframes: when the last one left it, and whether a
/// request for the next one is overdue an answer.
///
/// Bundled with [`KeyframeAnswerWatch`] rather than passed alongside it because the two are
/// updated together at every call site that touches either -- a keyframe leaving resets
/// both the cadence timer and the watch, and a request checks both the rate limit and the
/// watch -- and keeping them as one parameter is what keeps the encode functions under the
/// workspace's argument-count limit.
struct KeyframeState {
    last_keyframe: std::time::Instant,
    watch: KeyframeAnswerWatch,
}

impl KeyframeState {
    fn new() -> Self {
        Self {
            last_keyframe: std::time::Instant::now(),
            watch: KeyframeAnswerWatch::default(),
        }
    }
}

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

/// The ceiling a run may raise the encode rate to.
///
/// A cap on the cap: the value comes from the environment, and a typo that asked for 6000
/// would spend the whole machine on encoding before anyone noticed the extra zero.
const MAX_ENCODE_FPS_CEILING: u64 = 120;

/// The fastest this run will encode.
///
/// [`MAX_ENCODE_FPS`] is the default and the right one for a talking head. Nothing in
/// WebRTC, VP8, H.264 or the SFU requires it -- the camera here delivers 60 -- so
/// `ELEMENTIUM_MAX_FPS` raises or lowers it for a run that wants to trade bandwidth and CPU
/// for smoothness. Read once, because the encoder's frame interval and its bitrate are both
/// derived from it and they must not disagree.
fn max_encode_fps() -> u64 {
    static RESOLVED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *RESOLVED.get_or_init(|| {
        let Some(raw) = std::env::var_os("ELEMENTIUM_MAX_FPS") else {
            return MAX_ENCODE_FPS;
        };
        let parsed = raw.to_str().and_then(|v| v.parse::<u64>().ok());
        match parsed {
            Some(fps) if (1..=MAX_ENCODE_FPS_CEILING).contains(&fps) => {
                tracing::info!(fps, "encoding frame rate set from ELEMENTIUM_MAX_FPS");
                fps
            }
            _ => {
                tracing::warn!(
                    value = ?raw,
                    ceiling = MAX_ENCODE_FPS_CEILING,
                    default_fps = MAX_ENCODE_FPS,
                    "ELEMENTIUM_MAX_FPS is not a frame rate this will accept; using the default"
                );
                MAX_ENCODE_FPS
            }
        }
    })
}

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

/// [`max_encode_fps`] as the width the capture API takes.
fn max_encode_fps_u32() -> u32 {
    u32::try_from(max_encode_fps()).unwrap_or(30)
}

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
        return max_encode_fps_u32();
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
        .unwrap_or_else(max_encode_fps_u32)
}

/// Minimum gap between encoded frames, from [`max_encode_fps`].
fn min_encode_interval() -> std::time::Duration {
    std::time::Duration::from_nanos(1_000_000_000_u64.saturating_div(max_encode_fps().max(1)))
}

/// How early a frame may arrive and still count as this slot's frame, as a fraction of the
/// interval. A quarter of a frame at 30fps is 8ms.
const ENCODE_EARLY_TOLERANCE_DIVISOR: u32 = 4;

/// Decides which captured frames reach the encoder, holding the encode rate at or under the
/// cap without discarding frames merely for arriving a little early.
///
/// The rule this replaces was `last_encode.elapsed() >= interval`, resetting `last_encode`
/// to the moment of the encode. Both halves of that are wrong against a real camera:
///
///   - A camera nominally at the cap does not deliver on an exact period. Frames land a few
///     milliseconds either side, and every frame landing early was thrown away whole -- so a
///     30fps camera under a 30fps cap lost frames it had no business losing.
///   - Resetting to the encode time pushes the next deadline out by a full interval from
///     wherever the last frame happened to land, so the schedule drifts away from the camera
///     and the losses become periodic rather than occasional.
///
/// Measured on a real call: 4800 frames captured, 2862 sent, steady at 59.6% across every
/// reporting window. The missing 40% were counted nowhere -- the discard sat above every
/// counter in `encode_and_send` -- so the log showed a healthy pipeline sending 18fps.
///
/// Here the deadline advances by exactly one interval from the previous *deadline*, so the
/// cadence cannot drift, and is floored one interval behind the present so a stalled camera
/// cannot bank credit and emit a burst on recovery.
struct EncodePacer {
    interval: std::time::Duration,
    next_due: std::time::Instant,
}

impl EncodePacer {
    const fn new(interval: std::time::Duration, now: std::time::Instant) -> Self {
        Self {
            interval,
            next_due: now,
        }
    }

    /// Whether the frame that arrived at `now` should be encoded.
    fn admit(&mut self, now: std::time::Instant) -> bool {
        let tolerance = self
            .interval
            .checked_div(ENCODE_EARLY_TOLERANCE_DIVISOR)
            .unwrap_or_default();
        let earliest_accepted = self.next_due.checked_sub(tolerance).unwrap_or(self.next_due);
        if now < earliest_accepted {
            return false;
        }
        // Advance from the previous deadline so the cadence does not drift -- but pull a
        // deadline left behind by a stall up to one interval into the past first, or the
        // schedule would owe us frames and let a burst through on recovery.
        let base = self
            .next_due
            .max(now.checked_sub(self.interval).unwrap_or(now));
        self.next_due = base.checked_add(self.interval).unwrap_or(now);
        true
    }
}

/// Sane bounds for any bitrate this app hands an encoder, whether picked automatically by
/// [`bitrate_for`] or requested later through `setParameters`.
///
/// 300 is the floor a tiny frame still needs so the picture is not pure noise. 4000 is the
/// ceiling because [`bitrate_for`] already lands at roughly 2.7Mbps for 720p -- the highest
/// resolution this app captures -- under its own ~0.1-bit-per-pixel-per-frame formula, so
/// 4Mbps leaves headroom for a generous request without accepting a rate this app has never
/// had reason to send.
const MIN_BITRATE_KBPS: u32 = 300;
const MAX_BITRATE_KBPS: u32 = 4000;

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
    let bits_per_second = pixels.saturating_mul(max_encode_fps()).saturating_div(10);
    let kbps = bits_per_second.saturating_div(1000);
    u32::try_from(kbps.clamp(u64::from(MIN_BITRATE_KBPS), u64::from(MAX_BITRATE_KBPS)))
        .unwrap_or(2000)
}

/// The policy behind `RTCRtpSender.setParameters`: the maximum `maxBitrate` across the
/// supplied encodings, converted from bits per second to kbps.
///
/// A pure function over already-parsed values, not the encodings themselves, so it can be
/// unit-tested without a `State<MediaState>` or a running pipeline. `None` for any encoding
/// means it carried no `maxBitrate` and contributes nothing -- per policy, an encoding with
/// no cap is not a request to remove one. `None` overall (every encoding lacked a cap, or
/// there were no encodings) means "change nothing", which the caller must not mistake for
/// "set it to zero".
///
/// livekit-client does not do simulcast in this app -- there is no dynamic resize path to
/// drive extra layers -- so more than one encoding collapses to a single aggregate cap
/// rather than being rejected; the caller logs that once.
fn requested_bitrate_kbps(max_bitrates_bps: &[Option<u32>]) -> Option<u32> {
    max_bitrates_bps
        .iter()
        .filter_map(|maybe_bps| *maybe_bps)
        .max()
        .map(|bps| bps.saturating_div(1000))
}

/// Clamp a requested bitrate to [`MIN_BITRATE_KBPS`]..=[`MAX_BITRATE_KBPS`], reporting
/// whether clamping changed it.
///
/// Policy 5: `setParameters` is caller-supplied input, and a caller (or a bug in one) asking
/// for 0 or a few hundred megabits must not reach the encoder unfiltered.
fn clamp_bitrate_kbps(kbps: u32) -> (u32, bool) {
    let clamped = kbps.clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS);
    (clamped, clamped != kbps)
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
        /// The `PipeWire` node the user picked, when they picked one.
        ///
        /// Parsed from the id [`enumerate_devices`] handed out, so the picker and capture
        /// name the same thing. They used to disagree entirely -- the picker listed
        /// nokhwa's devices and capture walked `PipeWire`'s own list -- which is why
        /// choosing a camera in settings did nothing.
        node_id: Option<u32>,
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
            Self::Camera { node_id, width, height, fps } => {
                VideoSource::start_at_device(*width, *height, *fps, target, *node_id)
            }
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
        .ipc_err("get_display_media", source_id)?;

    // Taken before the capturer moves into the mutex, because this is the only thing that
    // makes an X11 share's failures visible to anyone. Every frame that fails to capture
    // used to be discarded with `.ok()?` -- no log, no counter, nothing `source_died()`
    // could see -- so a share could go dark mid-call and the pipeline would go on believing
    // it was healthy. The capturer now latches this flag after sustained failure; without
    // reading it here, that latch would be written and never read.
    let failed = capturer.failed_handle();

    let capturer = Arc::new(Mutex::new(capturer));
    let stopper_capturer = Arc::clone(&capturer);
    let stopper: Box<dyn Fn() + Send + Sync> = Box::new(move || {
        if let Ok(mut c) = stopper_capturer.lock() {
            let _ = c.stop();
        }
    });

    Ok(
        elementium_media::video_source::VideoSource::start_push_with_health(
            rx, stopper, "x11", failed,
        ),
    )
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

/// Whether a captured frame should be dropped because the user has this track muted.
///
/// Frames are drained from the capture first and discarded here. Draining keeps the
/// source's queue from backing up, so unmuting resumes on a live frame rather than
/// replaying a stale one; discarding here rather than at the channel means a muted track
/// also stops spending CPU encoding what nobody will receive.
fn dropped_because_muted(muted: &Arc<std::sync::atomic::AtomicBool>) -> bool {
    muted.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether the capture has failed outright, reported once with what it was feeding.
///
/// A shared window closing, or a monitor being unplugged, errors the stream rather than
/// ending it: `try_recv` simply keeps returning `None`, which is also what a perfectly
/// healthy screencast of a static window does between damage events. Without this the
/// pipeline spins forever on a dead source, publishing nothing, logging nothing, and
/// looking to every counter like an idle share.
fn source_died(
    capturer: &elementium_media::video_source::VideoSource,
    source: &VideoCaptureSource,
    track_id: &str,
    frame_count: u64,
) -> bool {
    if !capturer.failed() {
        return false;
    }
    tracing::error!(
        track_id = %track_id,
        source = source.label(),
        frame_count,
        "video capture failed; the source is gone and the pipeline is stopping"
    );
    true
}

/// The encode target for this source, given the codec currently negotiated.
///
/// Which format is worth asking the source for depends on where the frames will be
/// encoded, and the answer is not a small difference: MJPEG is the most expensive format on
/// offer when the CPU decodes it and the cheapest when the GPU does.
fn negotiated_target(
    source: &VideoCaptureSource,
    active_codec: &Arc<ActiveCodec>,
) -> elementium_codec::EncodeTarget {
    let (neg_width, neg_height) = source.negotiation_geometry();
    elementium_codec::EncodeTarget::negotiated(active_codec.get(), neg_width, neg_height)
}




/// Background thread: reads frames from a video source, writes RGBA to `VideoFrameBuffer`
/// for preview, and optionally encodes + sends them to a peer connection.
///
/// Serves the camera and the screen share both. They differ in how the source is opened and
/// in nothing else, so a second copy of this loop would only be a second place for every
/// future encode fix to be applied -- or forgotten.
/// Report a camera that would not start, naming whoever already has it.
///
/// "Device or resource busy" is true and unactionable, and a camera that will not start is
/// nearly always a camera another application already holds -- which the operating system
/// knows and we were not asking. A real instance of this took a round trip through the
/// logs to establish that Signal had the webcam open.
fn report_capture_failure(reason: &dyn std::fmt::Display, track_id: &str, source: &str) {
    let held_by = elementium_media::device_holders::holders_of("/dev/video")
        .iter()
        .map(elementium_media::device_holders::DeviceHolder::describe)
        .collect::<Vec<_>>()
        .join(", ");
    if held_by.is_empty() {
        tracing::error!(
            reason = %reason,
            track_id,
            source,
            "failed to start video capture"
        );
    } else {
        tracing::error!(
            reason = %reason,
            track_id,
            source,
            camera_held_by = %held_by,
            "failed to start video capture: another application is using the camera"
        );
    }
}

/// Live video-pipeline state that changes while capture keeps running -- the SFU's keyframe
/// requests, its codec choice, and now a caller's `setParameters` bitrate.
///
/// Bundled into one struct, the same reasoning as `PipelineExtras::Video` itself: each field
/// is a level rather than an event (only the latest value ever matters), and grouping them
/// keeps `video_pipeline_loop` and `encode_and_send_video_frame` inside the workspace's
/// seven-argument limit as this list grows, rather than reaching for another `#[allow]`.
struct VideoPipelineControls {
    keyframe_requested: Arc<std::sync::atomic::AtomicBool>,
    active_codec: Arc<ActiveCodec>,
    bitrate_override: Arc<std::sync::atomic::AtomicU32>,
}

#[allow(clippy::too_many_arguments)]
fn video_pipeline_loop(
    key: MediaTrackKey,
    muted: &Arc<std::sync::atomic::AtomicBool>,
    track_id: &str,
    source: &VideoCaptureSource,
    video_frames: &VideoFrameBuffer,
    encode_tx: &Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
    controls: &VideoPipelineControls,
    stop_rx: &std::sync::mpsc::Receiver<()>,
) {
    // Which format is worth asking the source for depends on where the frames will be
    // encoded, and the answer is not a small difference: MJPEG is the most expensive format
    // on offer when the CPU decodes it and the cheapest when the GPU does.
    let target = negotiated_target(source, &controls.active_codec);
    // Once per pipeline, so "is the GPU doing this" is answerable by reading a log rather
    // than by re-deriving the selection policy from the codec, the geometry and what the
    // driver reported. The three fields are what the policy actually decided on, and
    // `capture_preference` is what it will ask the source for as a result.
    tracing::info!(
        codec = controls.active_codec.get().sdp_name(),
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
            report_capture_failure(&e, track_id, source.label());
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
    let mut out = VideoOutState {
        keyframe: KeyframeState::new(),
        stats: OutboundVideoStats::default(),
        applied_bitrate_kbps: 0,
    };
    let mut frame_count: u64 = 0;
    let mut keyframe = KeyframeState::new();
    let mut last_preview = std::time::Instant::now()
        .checked_sub(MIN_PREVIEW_INTERVAL)
        .unwrap_or_else(std::time::Instant::now);
    let mut pacer = EncodePacer::new(min_encode_interval(), std::time::Instant::now());

    loop {
        if stop_rx.try_recv().is_ok() {
            tracing::info!(track_id = %track_id, "video pipeline stopping");
            release_preview(video_frames, track_id);
            break;
        }

        if source_died(&capturer, source, track_id, frame_count) {
            release_preview(video_frames, track_id);
            break;
        }

        if let Some(frame) = capturer.try_recv() {
            frame_count = frame_count.wrapping_add(1);
            if dropped_because_muted(muted) {
                continue;
            }
            out.count_captured(track_id, controls.active_codec.get());
            if frame_count <= 3 || frame_count.is_multiple_of(100) {
                // Named by source, not "Camera": this loop serves the screen share too, and
                // this is the one line that says frames are flowing. Calling a share's
                // frames camera frames means anyone grepping a log for a share's health
                // finds camera lines, or nothing, and concludes the capture is dead.
                tracing::info!(
                    track_id = %track_id,
                    source = source.label(),
                    frame_count,
                    w = frame.width(),
                    h = frame.height(),
                    compressed = frame.mjpeg().is_some(),
                    "capture frame received"
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
            // Counted when it is not, because a frame dropped here used to be invisible --
            // see `EncodePacer`.
            if should_encode && !pacer.admit(std::time::Instant::now()) {
                out.stats.paced_out = out.stats.paced_out.saturating_add(1);
            } else if should_encode {
                encode_and_send_video_frame(
                    PipelineId { key, track_id },
                    &frame,
                    &mut encoder,
                    &mut out,
                    controls,
                    encode_tx,
                );
            }
        } else {
            // The unanswered-keyframe clock runs here too, not only where frames are
            // encoded, because the case it exists for is precisely the one with no frames.
            // A screen share emits on damage: a still window can go many seconds without
            // producing anything, and a receiver asking during that window gets nothing and
            // would never be warned about, since every other check sits on the per-frame
            // path. That is the exact shape of the failure this watch was added for.
            keyframe.watch.check_timeout(track_id);
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
    keyframe: &mut KeyframeState,
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

    // Refused here rather than handed to the codec, because the codec's answer is true and
    // useless: `Failed to initialize VP8 encoder width=0 height=0` says nothing about *why*
    // the geometry is zero, and this session lost time to exactly that line while the real
    // fault was upstream — frames failing to convert, so nothing ever set a size. Naming
    // the cause here points at the capture rather than at the encoder.
    if width == 0 || height == 0 {
        tracing::error!(
            width,
            height,
            codec = wanted.sdp_name(),
            "refusing to build an encoder with no geometry; the capture has not produced a \
             frame with a size yet, so look upstream of the encoder"
        );
        return;
    }

    // Odd geometry never reaches here from the capture path, which crops to even at
    // negotiation, but an encoder that is handed it anyway fails at construction with
    // "VP8 requires even dimensions" — a failure the caller cannot act on mid-call.
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        tracing::error!(
            width,
            height,
            codec = wanted.sdp_name(),
            "refusing to build an encoder for odd geometry; VP8 cannot encode it, and the \
             capture should have cropped it to even before this point"
        );
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
            keyframe.last_keyframe = std::time::Instant::now();
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
    keyframe: &mut KeyframeState,
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
    let recently = keyframe.last_keyframe.elapsed() < MIN_KEYFRAME_GAP;
    if (asked && !recently) || keyframe.last_keyframe.elapsed() >= KEYFRAME_INTERVAL {
        encoder.request_keyframe();
        keyframe.last_keyframe = std::time::Instant::now();
        tracing::info!(track_id, on_request = asked, "requested a video keyframe");
        // Only a real receiver request opens a watch episode -- the periodic branch above
        // asks the encoder proactively, on our own schedule, and has no requester waiting
        // on it that could go unanswered.
        if asked {
            keyframe.watch.requested();
        }
    }
    // Checked every call, not only when a request was just forwarded: a request rate-
    // limited by `recently` above still leaves an earlier episode open, and that episode's
    // clock needs to keep running even on the frames where nothing new happens.
    keyframe.watch.check_timeout(track_id);
}

/// Encode one captured frame and hand the packets to the connection.
///
/// Generic over the encoder rather than taking a trait object: this runs for every
/// captured frame for the length of a call, so the calls are statically dispatched and
/// inlinable, and the bound documents that the body depends on nothing but the interface.
/// Encode one frame and hand its packets to the connection.
///
/// Returns whether any packet in this batch was a keyframe, which is the one fact
/// [`KeyframeAnswerWatch`] needs and the only place it is known -- the encoder's own
/// `EncodedFrame::is_keyframe`, not anything inferred from bytes sent or acked downstream.
/// What happened to each captured video frame between the encoder and the wire.
///
/// Audio has had this for a while, and video did not, which is why a frozen remote picture
/// has been so much harder to explain than silent audio: an undecodable frame, an encoder
/// error and a dropped `try_send` all vanished into `tracing::debug!` or a discarded
/// `Result`, so "the picture stopped" had no counter anywhere behind it.
///
/// The three send failures are counted separately because they need opposite responses. A
/// full channel is back-pressure -- the consumer is alive and behind. A closed one means
/// the peer connection went away and nothing re-attached this pipeline, so every later
/// frame is wasted. Not connected at all is the ordinary state before a call starts, and
/// counting it with the others would make a healthy idle pipeline look broken.
/// How long a pipeline may run producing frames that reach no peer connection before that
/// is worth a warning, rather than the ordinary gap while a pipeline starts up or a
/// mid-call renegotiation is in flight.
///
/// A pipeline is legitimately unattached for a moment on every start and every device
/// swap -- see [`connection_for_new_pipeline`] -- and warning about that trains whoever
/// reads the log to expect and ignore this warning, which defeats the point of having it.
/// Set with headroom over the longest ordinary handover this file waits out on purpose
/// ([`AUDIO_HANDOVER_TIMEOUT`], 750ms) so a real outage is what crosses it.
const NOT_CONNECTED_WARN_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

/// How often the "reaching no peer connection" warning repeats while the outage continues.
///
/// The incident this exists for ran 45 seconds with nothing in the log but a counter
/// nobody was watching, climbing once per captured frame. Once at the start of the outage
/// and then here after is loud enough to notice on a scroll through the log and nowhere
/// near the per-frame noise that a warning inside this same hot loop has produced before.
const NOT_CONNECTED_WARN_EVERY: std::time::Duration = std::time::Duration::from_secs(15);

/// Tracks whether a capture pipeline has been producing frames that reach no peer
/// connection for long enough to be a fault worth a log line, not ordinary startup or
/// renegotiation churn -- and, once it has warned, keeps quiet until either the pipeline
/// recovers or the next repeat interval is due.
///
/// Shaped after [`KeyframeAnswerWatch`]: the decision of whether to warn *now* is a pure
/// function of counts and instants, kept apart from the act of warning, so it is testable
/// at an arbitrary age without a real pipeline, a real clock, or a test that sleeps past a
/// multi-second threshold.
#[derive(Default)]
struct NotConnectedWatch {
    /// When the current run of unattached frames began. `None` means the most recent frame
    /// reached a connection, so there is nothing outstanding to time.
    unattached_since: Option<std::time::Instant>,
    /// When this run last actually warned. `None` means it has not yet warned this run, so
    /// the first warning only waits on [`NOT_CONNECTED_WARN_AFTER`], not the repeat gap.
    last_warned: Option<std::time::Instant>,
}

impl NotConnectedWatch {
    /// Whether a run open for `open_for`, having last warned `since_warn` ago (`None` if it
    /// has never warned this run), is due another warning right now.
    const fn is_due(open_for: std::time::Duration, since_warn: Option<std::time::Duration>) -> bool {
        if open_for.as_millis() < NOT_CONNECTED_WARN_AFTER.as_millis() {
            return false;
        }
        match since_warn {
            None => true,
            Some(gap) => gap.as_millis() >= NOT_CONNECTED_WARN_EVERY.as_millis(),
        }
    }

    /// Record one frame that reached no connection. Returns whether to warn about it now.
    fn record_skip(&mut self, now: std::time::Instant) -> bool {
        let opened_at = *self.unattached_since.get_or_insert(now);
        let open_for = now.saturating_duration_since(opened_at);
        let since_warn = self.last_warned.map(|warned_at| now.saturating_duration_since(warned_at));
        if Self::is_due(open_for, since_warn) {
            self.last_warned = Some(now);
            true
        } else {
            false
        }
    }

    /// Record that a frame just reached a connection, closing out any open run.
    ///
    /// Closing on the first frame sent, not on the connection being merely attached, is
    /// deliberate: an attached channel that only ever fills up or gets dropped by the peer
    /// is not the recovery this exists to notice.
    const fn record_sent(&mut self) {
        self.unattached_since = None;
        self.last_warned = None;
    }
}

#[derive(Default)]
struct OutboundVideoStats {
    captured: u64,
    sent: u64,
    /// Frames the rate limiter held back to keep the encode rate at the cap.
    ///
    /// Expected to be non-zero whenever the camera runs faster than the cap -- that is the
    /// limiter working. It is here because it used to be absent: the discard happened above
    /// every other counter, so 40% of a real call's frames vanished between `captured` and
    /// `sent` with every drop counter reading zero. See `EncodePacer`.
    paced_out: u64,
    skipped_not_connected: u64,
    dropped_channel_closed: u64,
    dropped_channel_full: u64,
    encode_errors: u64,
    undecodable: u64,
    bytes_since_report: u64,
    /// Whether a sustained run of frames reaching no peer connection is currently open, and
    /// whether it has warned about it. See [`NotConnectedWatch`].
    unattached: NotConnectedWatch,
}

impl OutboundVideoStats {
    /// Every 300 frames: ten seconds at 30fps, and the same cadence the capture path's own
    /// cost report uses, so the two line up in a log.
    const REPORT_EVERY: u64 = 300;

    fn report_if_due(&mut self, track_id: &str, codec: elementium_codec::VideoCodec) {
        if !self.captured.is_multiple_of(Self::REPORT_EVERY) || self.captured == 0 {
            return;
        }
        // At info even when everything is fine: a healthy line is what makes an unhealthy
        // one legible, and the absence of a log is not evidence of anything.
        tracing::info!(
            track_id,
            codec = codec.sdp_name(),
            captured = self.captured,
            sent = self.sent,
            paced_out = self.paced_out,
            skipped_not_connected = self.skipped_not_connected,
            dropped_channel_closed = self.dropped_channel_closed,
            dropped_channel_full = self.dropped_channel_full,
            encode_errors = self.encode_errors,
            undecodable = self.undecodable,
            kbytes = self.bytes_since_report.saturating_div(1024),
            "outbound video"
        );
        self.bytes_since_report = 0;
    }
}

fn encode_and_send<E: VideoEncoder>(
    id: PipelineId<'_>,
    encoder: &mut E,
    frame: &elementium_media::captured_frame::CapturedFrame,
    encode_tx: &Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
    stats: &mut OutboundVideoStats,
) -> bool {
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
            stats.undecodable = stats.undecodable.saturating_add(1);
            tracing::debug!("captured frame could not be decoded");
            return false;
        };
        encoder.encode(&planar)
    };

    match outcome {
        Ok(packets) => {
            let produced_keyframe = packets.iter().any(|p| p.is_keyframe);
            let connected = encode_tx.lock().ok().and_then(|g| g.clone());
            match connected {
                None => {
                    stats.skipped_not_connected =
                        stats.skipped_not_connected.saturating_add(u64::try_from(packets.len()).unwrap_or(0));
                    if stats.unattached.record_skip(std::time::Instant::now()) {
                        tracing::warn!(
                            track_id = id.track_id,
                            skipped_not_connected = stats.skipped_not_connected,
                            "captured video is reaching no peer connection; the far end is \
                             receiving nothing from this track"
                        );
                    }
                }
                Some(tx) => {
                    for packet in packets {
                        let len = u64::try_from(packet.data.as_bytes().len()).unwrap_or(0);
                        match tx.try_send(IoCommand::WriteVideo(id.key, packet.data, codec)) {
                            Ok(()) => {
                                stats.sent = stats.sent.saturating_add(1);
                                stats.bytes_since_report =
                                    stats.bytes_since_report.saturating_add(len);
                                stats.unattached.record_sent();
                            }
                            Err(tokio_mpsc::error::TrySendError::Full(_)) => {
                                stats.dropped_channel_full =
                                    stats.dropped_channel_full.saturating_add(1);
                            }
                            Err(tokio_mpsc::error::TrySendError::Closed(_)) => {
                                stats.dropped_channel_closed =
                                    stats.dropped_channel_closed.saturating_add(1);
                            }
                        }
                    }
                }
            }
            produced_keyframe
        }
        Err(e) => {
            stats.encode_errors = stats.encode_errors.saturating_add(1);
            tracing::debug!("video encode error: {e}");
            false
        }
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

/// The per-frame state a video pipeline carries between frames.
///
/// Bundled rather than passed as two parameters because they are always used together and
/// the alternative was an eighth argument, which the workspace's own lint refuses -- for
/// the good reason that a call site with eight positional arguments is one transposition
/// away from a bug nothing catches.
struct VideoOutState {
    keyframe: KeyframeState,
    stats: OutboundVideoStats,
    /// The bitrate last pushed into *this* encoder instance, in kbps. `0` means "not applied
    /// yet". Tracked separately from `PipelineExtras::Video::bitrate_override` -- that is the
    /// caller's request, this is what the current encoder was actually told -- so a fresh
    /// encoder (which starts from `bitrate_for`'s default, not the override) picks the
    /// override back up instead of `apply_bitrate_override` mistaking it for unchanged.
    applied_bitrate_kbps: u32,
}

impl VideoOutState {
    /// Count a frame that survived the mute check, and report the window if it is due.
    fn count_captured(&mut self, track_id: &str, codec: elementium_codec::VideoCodec) {
        self.stats.captured = self.stats.captured.saturating_add(1);
        self.stats.report_if_due(track_id, codec);
    }
}

/// Push a `setParameters`-requested bitrate into the encoder if it is not already applied.
///
/// Polled once per encoded frame rather than acted on the moment `setParameters` runs,
/// because the encoder lives on this thread and nothing else may touch it -- the same
/// level-not-event handling as `keyframe_requested` and `active_codec`. An atomic load is
/// cheap at the encoder's own rate; a channel would need its own draining for what is only
/// ever "the latest requested value".
fn apply_bitrate_override<E: VideoEncoder>(
    enc: &mut E,
    bitrate_override: &Arc<std::sync::atomic::AtomicU32>,
    applied: &mut u32,
    track_id: &str,
) {
    let requested = bitrate_override.load(std::sync::atomic::Ordering::Relaxed);
    if requested == 0 || requested == *applied {
        return;
    }
    match enc.set_bitrate(requested) {
        Ok(()) => {
            *applied = requested;
            tracing::info!(track_id, kbps = requested, "applied setParameters bitrate to encoder");
        }
        Err(e) => {
            // Recorded as applied even though it was not, so a rejecting encoder reports
            // once for this value instead of once per frame -- thirty errors a second is
            // how a log stops being read. A later, different request still retries, which
            // is the case worth retrying.
            *applied = requested;
            tracing::error!(
                track_id,
                kbps = requested,
                "encoder rejected the bitrate requested via setParameters: {e}"
            );
        }
    }
}

/// Encode one captured frame, keeping the encoder valid and honouring keyframe requests.
fn encode_and_send_video_frame(
    id: PipelineId<'_>,
    frame: &elementium_media::captured_frame::CapturedFrame,
    encoder: &mut Option<NegotiatedEncoder>,
    out: &mut VideoOutState,
    controls: &VideoPipelineControls,
    encode_tx: &Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
) {
    let wanted = controls.active_codec.get();
    let existed = encoder.as_ref().is_some_and(|e| e.codec() == wanted);
    ensure_encoder(encoder, frame.width(), frame.height(), &mut out.keyframe, wanted);
    if !existed {
        // A freshly built encoder starts from `bitrate_for`'s default, not the override, so
        // the tracked "already applied" value has to forget whatever the previous encoder
        // instance had -- otherwise a same-numbered override looks unchanged and never
        // reaches the new encoder.
        out.applied_bitrate_kbps = 0;
    }

    let Some(enc) = encoder.as_mut() else {
        return;
    };
    // A freshly built encoder emits a keyframe on its own, so only ask when it is one we
    // were already using.
    if existed {
        maybe_request_keyframe(id.track_id, enc, &mut out.keyframe, &controls.keyframe_requested);
    }
    apply_bitrate_override(
        enc,
        &controls.bitrate_override,
        &mut out.applied_bitrate_kbps,
        id.track_id,
    );
    if encode_and_send(id, enc, frame, encode_tx, &mut out.stats) {
        out.keyframe.watch.observed_keyframe();
    }
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
    /// The level the fold last saw on each input channel of the capture device.
    ///
    /// Reported because "which input is the microphone actually plugged into" is otherwise
    /// unanswerable from a log, and getting it wrong sounds exactly like a quiet room.
    channel_peaks: Vec<f32>,
    /// The gain automatic gain control is applying, 1.0 when it is off or idle.
    applied_gain: f32,
    /// The level it is judging that gain from.
    gain_envelope: f32,
    silent_packets_since_report: u64,
    /// Whether a sustained run of frames reaching no peer connection is currently open, and
    /// whether it has warned about it. See [`NotConnectedWatch`].
    unattached: NotConnectedWatch,
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

    /// Remember the gain the AGC settled on, and the level it judged it from.
    const fn record_gain(&mut self, gain: f32, envelope: f32) {
        self.applied_gain = gain;
        self.gain_envelope = envelope;
    }

    /// Remember the fold's view of each input channel for the next report.
    fn record_channel_peaks(&mut self, peaks: &[f32]) {
        self.channel_peaks.clear();
        self.channel_peaks.extend_from_slice(peaks);
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
                channel_peaks = ?self.channel_peaks,
                applied_gain = self.applied_gain,
                gain_envelope = self.gain_envelope,
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
        if stats.unattached.record_skip(std::time::Instant::now()) {
            tracing::warn!(
                track = %key,
                skipped_not_connected = stats.skipped_not_connected,
                "captured audio is reaching no peer connection; the far end is receiving \
                 nothing from this track"
            );
        }
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
                Delivery::Sent => {
                    stats.sent = stats.sent.saturating_add(1);
                    stats.unattached.record_sent();
                }
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

/// Opus only accepts 8/12/16/24/48kHz. A device that already opens at one of those keeps
/// its rate unchanged (48000 is the common case and must pass through untouched); anything
/// else -- 44100, 96000, 88200, 32000, 22050, 11025, ... -- is mapped to 48000, which is
/// only correct paired with an actual resample of the captured samples (see
/// `audio_capture_loop`, which resamples whenever the result of this function differs from
/// the device's rate).
const fn select_opus_rate(sample_rate: u32) -> u32 {
    match sample_rate {
        8000 | 12000 | 16000 | 24000 | 48000 => sample_rate,
        _ => 48000,
    }
}

/// Background thread: captures mic audio, Opus-encodes, and sends to a peer
/// connection when `encode_tx` is connected (deferred connection pattern).
fn audio_capture_loop(
    key: MediaTrackKey,
    device_index: Option<usize>,
    auto_gain: bool,
    encode_tx: &Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
    muted: &Arc<std::sync::atomic::AtomicBool>,
    handoff: &AudioCaptureHandoff,
    loss_estimate: &Arc<NetworkLossEstimate>,
) {
    let capturer = match AudioCapturer::start_on_device(device_index) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to start audio capture: {e}");
            return;
        }
    };

    let sample_rate = capturer.sample_rate();
    let channels = capturer.channels();

    let opus_rate = select_opus_rate(sample_rate);

    let encoder_config = OpusEncoderConfig::default();
    let mut encoder = match OpusEncoder::with_config(opus_rate, OUTBOUND_CHANNELS, encoder_config) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("Failed to create Opus encoder: {e}");
            return;
        }
    };

    // Logged once at startup, not just on mismatch: a silent rate mismatch is exactly what
    // let unresampled 96k/88.2k/32k/22.05k/11.025k devices reach the encoder as if they were
    // 48k, which sounds like the "robotic"/wrong-speed fault this project has chased before.
    let resampling_active = sample_rate != opus_rate;
    tracing::info!(
        sample_rate,
        channels,
        opus_rate,
        encoded_channels = OUTBOUND_CHANNELS,
        resampling_active,
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
    // Carries the per-channel level history the fold decides on, so it must outlive the
    // callback rather than being rebuilt per buffer.
    let mut mono_fold = elementium_media::audio_capture::MonoFold::new(channels);
    // `None` when the caller opted out, so the samples are not touched at all rather than
    // passed through a gain of one.
    let mut auto_gain = auto_gain.then(elementium_media::auto_gain::AutoGain::new);

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
        if handoff.stop_rx.try_recv().is_ok() {
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
            // Dropped here, before resampling, encoding or any chance of reaching a peer
            // connection. A muted microphone that still publishes is the worst version of
            // this codebase's recurring failure: the user has an icon telling them they are
            // silent, and everyone else can hear them.
            if muted.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }
            let mut data = frame.data;

            // Point 1: exactly what the microphone produced, before we touch it.
            elementium_media::audio_debug_dump::maybe_dump(
                "capture-raw",
                sample_rate,
                channels,
                &data,
            );

            // Resample whenever the device's actual rate differs from the Opus rate we
            // picked above -- not just for the common 44.1k -> 48k case. Opus only accepts
            // 8/12/16/24/48k, so any other device rate (96k, 88.2k, 32k, 22.05k, 11.025k,
            // ...) was previously handed to the encoder unresampled, which the encoder then
            // treated as if it were `opus_rate`: same sample count, wrong rate, so the far
            // end hears it sped up or slowed down. `mono_fold`/`accumulator` below read
            // `data` *after* this reassignment, so they always see the resampled sample
            // count, not the captured one -- getting that ordering backwards would frame
            // Opus packets at the wrong length instead of just the wrong pitch.
            if resampling_active {
                data = elementium_media::audio_playback::resample_interleaved(
                    &data, channels, sample_rate, opus_rate,
                );
            }

            // Fold to mono before framing: the encoder, the RTP timeline and the SDP all
            // describe a single channel from here on. Only the channels carrying signal
            // are averaged -- see `MonoFold`, and the 6 dB an audio interface's unused
            // second input used to cost.
            let mut mono = mono_fold.fold(&data);
            stats.record_channel_peaks(mono_fold.channel_peaks());
            if let Some(agc) = auto_gain.as_mut() {
                agc.apply(&mut mono);
                stats.record_gain(agc.gain(), agc.envelope());
            }
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

    // Dropped explicitly, before the acknowledgement, rather than left to fall out of scope
    // at the end of the function: that ordering is what makes the send below a true promise
    // that the device is free, not just "we are about to try to free it." `AudioCapturer`'s
    // `Drop` impl logs this too, so a stall here is diagnosable from either side.
    drop(capturer);
    let _ = handoff.release_tx.send(());
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
    muted: &Arc<std::sync::atomic::AtomicBool>,
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
        if muted.load(std::sync::atomic::Ordering::Relaxed) {
            // Dropped before it is encoded, and before it can reach a peer connection: a
            // muted microphone that still publishes is the failure this exists to remove.
            continue;
        }
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
mod mute_flag_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::dropped_because_muted;

    /// The capture loops ask this before touching a frame, and the answer decides whether
    /// media reaches the wire. Muting was a local fact for a long time -- the icon changed
    /// and the microphone kept publishing -- so the flag having the sense it claims is
    /// worth pinning even though the function is one line.
    #[test]
    fn a_raised_flag_drops_the_frame_and_a_lowered_one_does_not() {
        let flag = Arc::new(AtomicBool::new(false));
        assert!(!dropped_because_muted(&flag), "an unmuted track must publish");

        flag.store(true, Ordering::Relaxed);
        assert!(dropped_because_muted(&flag), "a muted track must not publish");

        flag.store(false, Ordering::Relaxed);
        assert!(!dropped_because_muted(&flag), "unmuting must resume publishing");
    }

    /// The flag is shared with the capture thread by handle, so a change made through one
    /// clone has to be visible through another. If it were copied instead, muting would
    /// set a flag nobody reads -- which is precisely the bug, one level down.
    #[test]
    fn the_flag_is_shared_not_copied() {
        let flag = Arc::new(AtomicBool::new(false));
        let held_by_pipeline = Arc::clone(&flag);

        flag.store(true, Ordering::Relaxed);

        assert!(
            dropped_because_muted(&held_by_pipeline),
            "the capture loop must see a mute set through the handle the command holds"
        );
    }
}

#[cfg(test)]
mod microphone_resample_tests {
    use super::select_opus_rate;
    use elementium_media::audio_playback::resample_interleaved;

    /// Opus's five supported rates must pass through unchanged, or a capture at one of
    /// them would be resampled for no reason. 48000 is the common case and the one most
    /// likely to regress silently if this ever stopped being a no-op.
    #[test]
    fn a_device_already_at_an_opus_rate_needs_no_resample() {
        assert_eq!(select_opus_rate(48_000), 48_000);
        assert_eq!(select_opus_rate(16_000), 16_000);
        assert_eq!(select_opus_rate(8_000), 8_000);
        assert_eq!(select_opus_rate(12_000), 12_000);
        assert_eq!(select_opus_rate(24_000), 24_000);
    }

    /// This was the one pairing the old code actually resampled, so it has to keep working
    /// exactly as before while the fix widens the condition around it.
    #[test]
    fn a_44_1k_device_maps_to_48k_and_needs_resampling() {
        let opus_rate = select_opus_rate(44_100);
        assert_eq!(opus_rate, 48_000);
        assert_ne!(44_100, opus_rate, "44.1k must not reach the encoder unresampled");
    }

    /// These rates fell through the old match arm with no resample at all: real device
    /// rate handed to an encoder that believed it was 48kHz. That is the actual bug --
    /// pinning that each of these is still recognised as needing a resample is the
    /// regression test for it.
    #[test]
    fn unsupported_rates_above_and_below_44_1k_also_need_resampling() {
        for device_rate in [96_000_u32, 88_200, 32_000, 22_050, 11_025] {
            let opus_rate = select_opus_rate(device_rate);
            assert_eq!(opus_rate, 48_000, "every unsupported rate maps to 48k");
            assert_ne!(
                device_rate, opus_rate,
                "{device_rate} must be flagged for resampling, not passed through raw"
            );
        }
    }

    /// A 20ms Opus frame is a fixed sample count at whatever rate Opus ends up encoding at.
    /// Resampling must land on exactly that count, not just something close to it -- a
    /// one-sample drift here means every accumulated frame is the wrong length, which is
    /// worse than the unresampled bug this replaces. The frame-duration algebra actually
    /// cancels out the rate ratio exactly (`in_frames` samples times `to_rate / from_rate`
    /// equals `to_rate`'s own 20ms sample count), so every case below lands on the same 960
    /// samples that 48kHz uses natively.
    #[test]
    fn a_20ms_frame_resamples_to_the_opus_rates_own_20ms_frame_size() {
        const FRAME_MS: u32 = 20;
        let samples_per_20ms = |rate: u32| -> usize {
            usize::try_from(rate.saturating_mul(FRAME_MS) / 1000).unwrap_or(0)
        };

        for device_rate in [48_000_u32, 44_100, 96_000, 16_000, 22_050] {
            let opus_rate = select_opus_rate(device_rate);
            let input_samples = samples_per_20ms(device_rate);
            let expected_output_samples = samples_per_20ms(opus_rate);

            let input = vec![0.0_f32; input_samples];
            let output = resample_interleaved(&input, 1, device_rate, opus_rate);

            assert_eq!(
                output.len(),
                expected_output_samples,
                "device_rate={device_rate} opus_rate={opus_rate} must produce a full 20ms \
                 Opus frame, not a partial or oversized one"
            );
        }
    }
}

#[cfg(test)]
mod camera_device_id_tests {
    use super::camera_node_id;

    /// The picker's id has to resolve back to the node capture will open, or choosing a
    /// camera does nothing -- which is exactly what it did before these ids agreed.
    #[test]
    fn a_pipewire_device_id_resolves_to_its_node() {
        assert_eq!(camera_node_id(Some("video-input-pw-247")), Some(247));
    }

    /// The nokhwa fallback ids name an index into a different enumeration. Parsing one as a
    /// node id would open an unrelated camera with great confidence, so they resolve to
    /// nothing and capture keeps its "first source that works" behaviour.
    #[test]
    fn a_fallback_device_id_resolves_to_nothing() {
        assert_eq!(camera_node_id(Some("video-input-3")), None);
        assert_eq!(camera_node_id(Some("")), None);
        assert_eq!(camera_node_id(None), None);
    }

    /// A malformed id is refused rather than partially parsed.
    #[test]
    fn a_malformed_pipewire_id_resolves_to_nothing() {
        assert_eq!(camera_node_id(Some("video-input-pw-")), None);
        assert_eq!(camera_node_id(Some("video-input-pw-abc")), None);
    }
}

/// Pins the ordering fix for a real fault: a microphone restart used to spawn the new
/// capture thread (which opens the device again) the instant `stop_tx` was sent, with no
/// wait for the old thread to drop its `cpal::Stream`. Two streams briefly held the same
/// input, and the loser was fed nothing -- `input_peak`/`channel_peaks` pinned at exactly
/// zero from its first frame while `sent` kept climbing, so the far end heard silence with
/// a perfectly healthy-looking pipeline. `wait_for_capturer_release` is the piece of that
/// fix that can be tested without real audio hardware: whether a pipeline restart blocks on
/// the previous capturer's acknowledgement, and gives up after a bounded timeout rather than
/// hanging forever if it never comes. The full end-to-end claim -- that two `cpal::Stream`s
/// can no longer contend for one physical device -- is not testable here: that requires a
/// real input device and observing what cpal actually delivers to two overlapping streams,
/// which is exactly the scenario this fix prevents from ever occurring in the running
/// pipeline, not something a unit test can construct.
#[cfg(test)]
mod audio_handover_tests {
    use super::wait_for_capturer_release;
    use std::time::Duration;

    /// The common case: the old capturer drops and acknowledges well inside the timeout.
    #[test]
    fn returns_true_once_the_acknowledgement_arrives() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            let _ = tx.send(());
        });
        assert!(
            wait_for_capturer_release(&rx, Duration::from_millis(500)),
            "an acknowledgement sent well inside the timeout must be observed"
        );
    }

    /// The fault this whole change addresses: nothing ever released the device. The wait
    /// must give up at the timeout rather than blocking the new pipeline forever.
    #[test]
    fn returns_false_and_does_not_exceed_the_timeout_when_no_acknowledgement_arrives() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        // Kept alive so `recv_timeout` times out rather than seeing the channel close.
        let _keep_open = tx;
        let timeout = Duration::from_millis(60);
        let started = std::time::Instant::now();
        let acked = wait_for_capturer_release(&rx, timeout);
        let elapsed = started.elapsed();
        assert!(!acked, "no acknowledgement was ever sent");
        assert!(
            elapsed < timeout.saturating_mul(3),
            "the wait must be bounded by the timeout, not hang: took {elapsed:?}"
        );
    }
}

#[cfg(test)]
mod keyframe_answer_watch_tests {
    use std::time::Duration;

    use super::{KEYFRAME_ANSWER_TIMEOUT, KeyframeAnswerWatch};

    /// The failure this exists for: requests keep arriving and nothing answers them. The
    /// watch must not speak on the first one -- that is indistinguishable from a keyframe
    /// that is merely late -- but it must accumulate a count so that when it does speak,
    /// the log says how many requests were folded into the episode rather than just "one
    /// happened, eventually".
    #[test]
    fn repeated_requests_without_an_answer_accumulate_into_one_episode() {
        let mut watch = KeyframeAnswerWatch::default();
        assert!(watch.pending_since.is_none(), "starts with nothing open");

        watch.requested();
        assert_eq!(watch.unanswered_requests, 1);
        let opened_at = watch.pending_since;
        assert!(opened_at.is_some());

        // Three more requests arrive before anything answers them -- the shape of a
        // receiver resending PLIs several times a second into a stuck encoder.
        watch.requested();
        watch.requested();
        watch.requested();
        assert_eq!(
            watch.unanswered_requests, 4,
            "every request in the open episode must be counted"
        );
        assert_eq!(
            watch.pending_since, opened_at,
            "the episode's start time must not move just because more requests arrived"
        );
    }

    /// A keyframe actually leaving the encoder is the only thing that should close an
    /// episode -- that is the fact the incident's logs never recorded, so it is the one
    /// the watch treats as authoritative.
    #[test]
    fn an_observed_keyframe_closes_the_episode() {
        let mut watch = KeyframeAnswerWatch::default();
        watch.requested();
        watch.requested();
        assert_eq!(watch.unanswered_requests, 2);

        watch.observed_keyframe();

        assert!(
            watch.pending_since.is_none(),
            "a produced keyframe must clear the open episode"
        );
        assert_eq!(watch.unanswered_requests, 0);
        assert!(!watch.warned, "closing an episode must not leave it warned");
    }

    /// The warning must actually fire once the episode has been open long enough, and then
    /// stay quiet.
    ///
    /// The agent-written tests covered accumulating, closing and *not* warning early, which
    /// together can all pass while the warning never fires at all -- the one behaviour the
    /// watch exists for.
    #[test]
    fn an_episode_open_past_the_timeout_is_due_exactly_once() {
        let mut watch = KeyframeAnswerWatch::default();
        assert!(!watch.is_due(KEYFRAME_ANSWER_TIMEOUT), "nothing is due with no episode open");

        watch.requested();
        assert!(
            !watch.is_due(KEYFRAME_ANSWER_TIMEOUT.saturating_sub(Duration::from_millis(1))),
            "a fresh episode must not warn"
        );
        assert!(watch.is_due(KEYFRAME_ANSWER_TIMEOUT), "an episode at the timeout is due");

        // `check_timeout` sets `warned`; simulate that, since the real call needs a clock.
        watch.warned = true;
        assert!(
            !watch.is_due(KEYFRAME_ANSWER_TIMEOUT.saturating_mul(10)),
            "an episode that already warned must not warn again, however long it stays open"
        );

        watch.observed_keyframe();
        watch.requested();
        assert!(
            watch.is_due(KEYFRAME_ANSWER_TIMEOUT),
            "a new episode after a keyframe must be able to warn again"
        );
    }

    /// `check_timeout` before [`super::KEYFRAME_ANSWER_TIMEOUT`] has elapsed must not fire:
    /// most requests are answered well inside it, and warning on every one would be the
    /// exact per-request noise this design avoids.
    #[test]
    fn a_fresh_request_does_not_warn_immediately() {
        let mut watch = KeyframeAnswerWatch::default();
        watch.requested();
        watch.check_timeout("track-1");
        assert!(
            !watch.warned,
            "a request that just arrived has not had time to prove it is unanswered"
        );
    }
}

#[cfg(test)]
mod not_connected_watch_tests {
    use std::time::{Duration, Instant};

    use super::{NOT_CONNECTED_WARN_AFTER, NOT_CONNECTED_WARN_EVERY, NotConnectedWatch};

    /// The startup/renegotiation gap this must not warn about: a pipeline is routinely
    /// unattached for a moment after `getUserMedia` or a device swap, well under
    /// `NOT_CONNECTED_WARN_AFTER`. A naive "warn on the first skip" implementation fails
    /// this immediately -- which is exactly the noise this design exists to avoid.
    #[test]
    fn a_brief_gap_does_not_warn() {
        let mut watch = NotConnectedWatch::default();
        let start = Instant::now();
        assert!(!watch.record_skip(start), "the very first skip must never warn on its own");
        assert!(
            !watch.record_skip(start + Duration::from_millis(500)),
            "a gap well under the threshold must stay quiet"
        );
    }

    /// The incident this exists for: once a run of skips has been open long enough, it must
    /// actually speak. A naive implementation that only records the skip without ever
    /// returning `true` (i.e. never warns) passes `a_brief_gap_does_not_warn` above but
    /// fails this -- so both are needed to pin the behaviour down.
    #[test]
    fn a_sustained_outage_warns_once_it_crosses_the_threshold() {
        let mut watch = NotConnectedWatch::default();
        let start = Instant::now();
        watch.record_skip(start);
        assert!(
            !watch.record_skip(
                start + NOT_CONNECTED_WARN_AFTER.saturating_sub(Duration::from_millis(1))
            ),
            "must not fire a moment before the threshold"
        );
        assert!(
            watch.record_skip(start + NOT_CONNECTED_WARN_AFTER),
            "must fire once the run has been open at least NOT_CONNECTED_WARN_AFTER"
        );
    }

    /// Once it has warned, it must not warn again on every subsequent skipped frame -- a
    /// pipeline producing frames at tens of times a second would otherwise turn one warning
    /// into the exact per-frame flood this file has already been bitten by once. It should,
    /// however, warn again after the repeat interval, since a 45-second outage deserves more
    /// than one line.
    #[test]
    fn a_continuing_outage_is_throttled_to_the_repeat_interval() {
        let mut watch = NotConnectedWatch::default();
        let start = Instant::now();
        assert!(!watch.record_skip(start), "the run must open before it can be due");
        assert!(watch.record_skip(start + NOT_CONNECTED_WARN_AFTER));

        // Every frame in between must stay quiet, not just the very next one.
        for step in 1..20 {
            let now = start + NOT_CONNECTED_WARN_AFTER + Duration::from_millis(step * 10);
            assert!(
                !watch.record_skip(now),
                "must not warn again before the repeat interval elapses"
            );
        }

        let repeat_due = start + NOT_CONNECTED_WARN_AFTER + NOT_CONNECTED_WARN_EVERY;
        assert!(
            watch.record_skip(repeat_due),
            "a still-open outage must warn again once the repeat interval has elapsed"
        );
    }

    /// Frames reaching a connection again must clear the state, so a recovered pipeline does
    /// not keep warning and a later, unrelated outage is judged on its own merits rather
    /// than inheriting an already-tripped `last_warned`.
    #[test]
    fn recovery_resets_the_run_so_a_later_outage_warns_again() {
        let mut watch = NotConnectedWatch::default();
        let start = Instant::now();
        assert!(!watch.record_skip(start), "the run must open before it can be due");
        assert!(watch.record_skip(start + NOT_CONNECTED_WARN_AFTER));

        watch.record_sent();
        assert!(watch.unattached_since.is_none(), "recovery must clear the open run");
        assert!(watch.last_warned.is_none(), "recovery must clear the warned state too");

        // A fresh outage starting right after recovery must go through the same quiet
        // period again, not warn immediately because a previous run once crossed it.
        let restart = start + NOT_CONNECTED_WARN_AFTER + Duration::from_secs(1);
        assert!(!watch.record_skip(restart), "a fresh outage must not warn on its first skip");
        assert!(
            watch.record_skip(restart + NOT_CONNECTED_WARN_AFTER),
            "but must still be able to warn once it, too, is sustained"
        );
    }
}

#[cfg(test)]
mod video_bitrate_tests {
    use super::{MAX_ENCODE_FPS, bitrate_for, max_encode_fps, min_encode_interval};

    /// The default has to hold when nothing asks for anything else, because every other
    /// number here is derived from it -- the frame interval and the bitrate both.
    #[test]
    fn the_default_frame_rate_is_the_one_documented() {
        // The environment is not set in the test runner, and this is read once per process.
        assert_eq!(max_encode_fps(), MAX_ENCODE_FPS);
    }

    /// The interval and the rate must agree, or the encoder is paced for one rate and given
    /// a bitrate budget for another.
    #[test]
    fn the_frame_interval_matches_the_rate() {
        let expected = 1_000_000_000_u64 / max_encode_fps();
        assert_eq!(u64::try_from(min_encode_interval().as_nanos()).unwrap_or(0), expected);
    }

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
                min_encode_interval()
                    .as_nanos()
                    .try_into()
                    .unwrap_or(u64::MAX),
            )
            .unwrap_or(0);
        assert_eq!(per_second, MAX_ENCODE_FPS);
    }
}

#[cfg(test)]
mod set_parameters_policy_tests {
    use super::{MAX_BITRATE_KBPS, MIN_BITRATE_KBPS, clamp_bitrate_kbps, requested_bitrate_kbps};

    /// The regression this whole feature exists to fix: `setParameters` used to resolve
    /// successfully and change nothing. Empty input is the shape a caller who set no
    /// `maxBitrate` at all produces, and it must say "change nothing" rather than "set it to
    /// zero" -- the two look identical to an encoder that just gets a `u32`.
    #[test]
    fn empty_input_changes_nothing() {
        assert_eq!(requested_bitrate_kbps(&[]), None);
    }

    /// An encoding with no `maxBitrate` contributes nothing to the aggregate -- it is not a
    /// request to remove an existing cap.
    #[test]
    fn missing_values_are_ignored() {
        assert_eq!(requested_bitrate_kbps(&[None, None]), None);
    }

    /// livekit-client's own congestion control lowers `maxBitrate` under a bad link and
    /// raises it as the link recovers. Only the highest of what was asked for should reach
    /// the encoder -- the maximum, not the first, the last, or an average.
    #[test]
    fn the_maximum_wins() {
        assert_eq!(
            requested_bitrate_kbps(&[Some(500_000), Some(2_000_000), Some(1_000_000)]),
            Some(2000),
            "2,000,000 bps is 2000 kbps, and it is the largest of the three"
        );
    }

    /// This app does not implement simulcast, so several encodings collapse to one aggregate
    /// cap rather than being rejected -- the maximum still wins even when some entries carry
    /// no cap at all.
    #[test]
    fn missing_values_do_not_prevent_a_real_one_from_winning() {
        assert_eq!(requested_bitrate_kbps(&[None, Some(1_500_000), None]), Some(1500));
    }

    /// bps to kbps is a division, not a re-derivation of the value -- a caller asking for
    /// 999 bps must not round up to a whole kbps that was never requested.
    #[test]
    fn bits_per_second_convert_down_to_kbps() {
        assert_eq!(requested_bitrate_kbps(&[Some(999)]), Some(0));
        assert_eq!(requested_bitrate_kbps(&[Some(1_000)]), Some(1));
    }

    /// A value already inside the range must pass through unchanged and unflagged --
    /// clamping every request, even sane ones, would make the "was this clamped" signal
    /// meaningless.
    #[test]
    fn a_sane_value_is_not_clamped() {
        assert_eq!(clamp_bitrate_kbps(2000), (2000, false));
    }

    /// The two ends of the range this feature exists to enforce: policy 5 says a caller's
    /// request must not reach the encoder unfiltered, in either direction.
    #[test]
    fn clamping_applies_at_both_ends() {
        assert_eq!(clamp_bitrate_kbps(0), (MIN_BITRATE_KBPS, true));
        assert_eq!(clamp_bitrate_kbps(50), (MIN_BITRATE_KBPS, true));
        assert_eq!(clamp_bitrate_kbps(50_000), (MAX_BITRATE_KBPS, true));
    }
}

#[cfg(test)]
mod requested_fps_tests {
    use super::{max_encode_fps_u32, requested_fps};

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
        assert_eq!(requested_fps(f64::NAN), max_encode_fps_u32());
        assert_eq!(requested_fps(f64::INFINITY), max_encode_fps_u32());
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

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod encode_pacer_tests {
    use super::EncodePacer;
    use std::time::{Duration, Instant};

    /// Feed frames at a fixed cadence and report how many the pacer admitted.
    fn admitted(interval: Duration, capture_gap: Duration, frames: u32) -> u32 {
        let start = Instant::now();
        let mut pacer = EncodePacer::new(interval, start);
        (0..frames)
            .filter(|i| pacer.admit(start + capture_gap * *i))
            .count()
            .try_into()
            .unwrap_or(u32::MAX)
    }

    /// A camera at the cap must lose nothing.
    ///
    /// The regression this exists for. With the old `elapsed() >= interval` rule and its
    /// reset-to-now, a 30fps camera under a 30fps cap lost roughly 40% of its frames
    /// because each one arrived a hair early, and the deadline then drifted further away
    /// on every encode. Measured on a real call: 4800 captured, 2862 sent.
    #[test]
    fn a_camera_running_at_the_cap_loses_nothing() {
        let interval = Duration::from_micros(33_333);
        assert_eq!(admitted(interval, interval, 100), 100);
    }

    /// The real cadence from the log: 29-30fps nominal, 25.8ms between frames.
    ///
    /// The pacer may hold some of these back -- the camera is running above the cap -- but
    /// it must not throw away two fifths of them.
    #[test]
    fn a_camera_running_slightly_fast_keeps_most_of_its_frames() {
        let admitted = admitted(
            Duration::from_micros(33_333),
            Duration::from_micros(25_800),
            100,
        );
        assert!(
            admitted >= 75,
            "kept only {admitted} of 100 frames from a 38fps camera under a 30fps cap"
        );
    }

    /// Jitter around the interval must not cost a frame either side.
    #[test]
    fn jitter_around_the_interval_does_not_drop_frames() {
        let interval = Duration::from_micros(33_333);
        let start = Instant::now();
        let mut pacer = EncodePacer::new(interval, start);
        let mut at = start;
        let mut kept = 0_u32;
        // Alternately 3ms early and 3ms late, averaging exactly the cap.
        for i in 0..100_u32 {
            let gap = if i % 2 == 0 {
                Duration::from_micros(30_333)
            } else {
                Duration::from_micros(36_333)
            };
            at += gap;
            if pacer.admit(at) {
                kept = kept.saturating_add(1);
            }
        }
        assert_eq!(kept, 100);
    }

    /// The cap is still a cap: a camera at twice the rate is halved, not passed through.
    #[test]
    fn a_camera_running_at_double_the_cap_is_held_to_it() {
        let interval = Duration::from_micros(33_333);
        let admitted = admitted(interval, Duration::from_micros(16_666), 100);
        assert!(
            (45..=60).contains(&admitted),
            "a 60fps camera under a 30fps cap admitted {admitted} of 100"
        );
    }

    /// A stall must not bank credit and then let a burst through.
    ///
    /// Without the floor, the deadline would sit a second in the past after a pause and
    /// admit every frame of the next second regardless of the cap.
    #[test]
    fn a_stalled_camera_does_not_burst_on_recovery() {
        let interval = Duration::from_micros(33_333);
        let start = Instant::now();
        let mut pacer = EncodePacer::new(interval, start);
        assert!(pacer.admit(start));

        // Nothing for a second, then frames as fast as the camera can manage.
        let resumed = start + Duration::from_secs(1);
        let mut kept = 0_u32;
        for i in 0..30_u32 {
            if pacer.admit(resumed + Duration::from_millis(1) * i) {
                kept = kept.saturating_add(1);
            }
        }
        assert!(
            kept <= 3,
            "a 30ms burst after a one-second stall admitted {kept} frames"
        );
    }
}
