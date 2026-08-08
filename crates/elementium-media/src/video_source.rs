//! Pick a working way to capture video, whatever the machine offers.
//!
//! Two capture paths exist and neither works everywhere. `PipeWire` is correct on a modern
//! Linux desktop and is the *only* option when the daemon holds the camera -- direct `V4L2`
//! then fails with `EBUSY` however the code is written. `V4L2` is the fallback for systems
//! with no `PipeWire` session, and remains the path on other platforms.
//!
//! Trying `PipeWire` first and falling back is deliberate: a `PipeWire`-managed camera
//! cannot be opened directly, but a directly-openable camera is almost always also visible
//! through `PipeWire`, so the reverse order would work by luck on some machines and fail on
//! others for reasons no log would explain.

use std::time::{Duration, Instant};

use crate::captured_frame::CapturedFrame;

use crate::camera::CameraCapturer;
use crate::pipewire_capture::PipewireCapturer;

/// How long a source has to produce its first frame.
///
/// Generous on purpose. A camera does not start when the format is agreed: an OBSBOT Tiny 2
/// on this machine negotiates in 170ms and then takes a further 1.9 seconds to deliver
/// anything, because the sensor has to wake, expose and settle. A tighter bound would give
/// up on a working camera and fall back for no reason, and the fallback is the worse path
/// -- it cannot be told which format to produce.
///
/// The cost of being generous is bounded: a stream that has actually failed reports an
/// error and is abandoned immediately, so this timeout is only ever waited out by a source
/// that is silent without saying why.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);

/// A running video capture, from whichever backend worked.
pub enum VideoSource {
    Pipewire(PipewireCapturer),
    V4l2(CameraCapturer),
}

impl VideoSource {
    /// Start capturing, preferring `PipeWire`.
    ///
    /// `width`/`height` are honoured only by the `V4L2` path; the `PipeWire` source
    /// negotiates its own geometry with the device and reports it per frame.
    ///
    /// # Errors
    ///
    /// Returns a description of *both* failures if neither backend works, because "the
    /// camera did not start" without saying what each path objected to is the exact
    /// unhelpfulness this module exists to remove.
    pub fn start(width: Option<u32>, height: Option<u32>) -> Result<Self, String> {
        Self::start_at(
            width,
            height,
            crate::pipewire_capture::DEFAULT_CAPTURE_FPS,
            elementium_codec::EncodeTarget::software(),
        )
    }

    /// Start capturing at a requested frame rate.
    ///
    /// The rate is a request: a source may only offer one rate, and one that offers more
    /// may still deliver more. What it does guarantee is that frames beyond it are dropped
    /// before being decoded, which is where the cost is.
    ///
    /// 30 suits a video call. Streaming and screen capture want 60 or more, which is why
    /// this exists rather than a constant.
    ///
    /// `target` says which encoder the frames are destined for, which decides the capture
    /// format worth asking for -- see [`crate::pipewire_capture::PipewireCapturer::start_at`].
    /// It reaches only the `PipeWire` path; the `V4L2` fallback takes what the device gives.
    ///
    /// # Errors
    ///
    /// As [`VideoSource::start`].
    pub fn start_at(
        width: Option<u32>,
        height: Option<u32>,
        target_fps: u32,
        target: elementium_codec::EncodeTarget,
    ) -> Result<Self, String> {
        let pipewire_err = match start_pipewire(target_fps, target) {
            Ok(source) => return Ok(source),
            Err(e) => e,
        };
        tracing::warn!(reason = %pipewire_err, "PipeWire capture unavailable, falling back to V4L2");

        match CameraCapturer::start(width, height) {
            Ok(c) => {
                tracing::info!(
                    width = c.width(),
                    height = c.height(),
                    "Camera capture started via V4L2"
                );
                Ok(Self::V4l2(c))
            }
            Err(v4l2_err) => Err(format!(
                "no camera could be started. PipeWire: {pipewire_err}. V4L2: {v4l2_err}"
            )),
        }
    }

    /// Start capturing a screen the `XDG` desktop portal has already granted.
    ///
    /// The camera paths above exist because *which* device to open is unknown until it is
    /// discovered by enumeration. Screen capture has no such question: the portal's picker
    /// dialog is the discovery step, and by the time it returns, `node_id` is the one and
    /// only source the user chose. Enumerating again here would be pointless -- there is
    /// nothing else to compare it against -- and would also be wrong, since a screencast
    /// node is not a camera and would not appear in [`crate::pipewire_nodes::list_video_sources`]
    /// in the first place.
    ///
    /// Everything below the node id is identical to the camera path: it is the same
    /// `PipeWire` variant, opened directly instead of found by search, because frames from a
    /// portal-granted node arrive over `PipeWire` exactly as camera frames do.
    ///
    /// # Errors
    ///
    /// Returns a description of why the `PipeWire` connection to `node_id` failed. There is
    /// no fallback to try, unlike [`VideoSource::start_at`] -- `V4L2` cannot open a node the
    /// portal handed out, so there is nothing a second attempt could do differently.
    pub fn start_screencast(
        node_id: u32,
        target: elementium_codec::EncodeTarget,
    ) -> Result<Self, String> {
        let capturer = PipewireCapturer::start_with(
            node_id,
            crate::pipewire_capture::CaptureRequest {
                target_fps: crate::pipewire_capture::DEFAULT_CAPTURE_FPS,
                target,
                profile: crate::pipewire_capture::SourceProfile::Screencast,
            },
        )
        .map_err(|e| e.to_string())?;
        Ok(Self::Pipewire(capturer))
    }

    /// The next frame, if one is waiting.
    ///
    /// Not always pixels: see [`CapturedFrame`]. The `V4L2` path always decodes, since
    /// `nokhwa` does it before we see the buffer, so only the `PipeWire` path can hand back
    /// compressed frames.
    #[must_use]
    pub fn try_recv(&self) -> Option<CapturedFrame> {
        match self {
            Self::Pipewire(c) => c.try_recv(),
            Self::V4l2(c) => c.try_recv().map(CapturedFrame::Planar),
        }
    }

    /// Frame size, once known.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        match self {
            Self::Pipewire(c) => c.size(),
            Self::V4l2(c) => (c.width(), c.height()),
        }
    }

    /// Which backend is in use, for logging.
    #[must_use]
    pub const fn backend(&self) -> &'static str {
        match self {
            Self::Pipewire(_) => "pipewire",
            Self::V4l2(_) => "v4l2",
        }
    }

    /// Stop capturing.
    pub fn stop(&self) {
        match self {
            Self::Pipewire(c) => c.stop(),
            Self::V4l2(c) => c.stop(),
        }
    }
}

/// Wait for one frame, or say why it will not come.
///
/// Gives up early when the stream reports an error rather than waiting out the timeout: a
/// stream that has failed will not recover, and every second spent here is a second the
/// camera is not running on the path that would have worked.
fn wait_for_first_frame(capturer: &PipewireCapturer) -> Result<(), String> {
    let deadline = Instant::now().checked_add(FIRST_FRAME_TIMEOUT);
    while deadline.is_some_and(|d| Instant::now() < d) {
        if capturer.try_recv().is_some() {
            return Ok(());
        }
        if capturer.failed() {
            return Err("the stream failed after connecting".to_owned());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let (width, height) = capturer.size();
    if width > 0 && height > 0 {
        return Err(format!(
            "negotiated {width}x{height} and then delivered no frames"
        ));
    }
    Err("negotiated no usable format".to_owned())
}

/// Start the first `PipeWire` source that actually delivers a frame.
///
/// A frame, not a format. Negotiating a size proves only that both ends agreed on what a
/// picture would look like, and a stream can do that and then fail on the buffers — which
/// is what an OBSBOT Tiny 2 does here, reporting a perfectly good 1280x720 and then
/// producing nothing at all. Accepting that leaves the camera silently dead: the log says
/// capture started, no frames ever arrive, and the `V4L2` path that would have worked is
/// never tried.
///
/// The cost is one frame, which is consumed to prove the stream is live and cannot be put
/// back. That is a fair price for knowing the difference between a working camera and a
/// convincing impression of one.
fn start_pipewire(
    target_fps: u32,
    target: elementium_codec::EncodeTarget,
) -> Result<VideoSource, String> {
    let sources = crate::pipewire_nodes::list_video_sources().map_err(|e| e.to_string())?;
    if sources.is_empty() {
        return Err("PipeWire offered no video sources".to_owned());
    }

    let mut last_error = String::new();
    for source in &sources {
        match PipewireCapturer::start_at(source.node_id, target_fps, target) {
            Ok(capturer) => match wait_for_first_frame(&capturer) {
                Ok(()) => {
                    let (width, height) = capturer.size();
                    tracing::info!(
                        node_id = source.node_id,
                        name = %source.description,
                        width,
                        height,
                        "Camera capture started via PipeWire"
                    );
                    return Ok(VideoSource::Pipewire(capturer));
                }
                Err(reason) => {
                    capturer.stop();
                    last_error =
                        format!("node {} ({}): {reason}", source.node_id, source.description);
                    tracing::warn!(reason = %last_error, "Skipping PipeWire source");
                }
            },
            Err(e) => {
                last_error = format!("node {}: {e}", source.node_id);
                tracing::warn!(reason = %last_error, "Skipping PipeWire source");
            }
        }
    }
    Err(last_error)
}
