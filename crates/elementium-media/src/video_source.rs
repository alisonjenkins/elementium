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

use elementium_types::I420Frame;

use crate::camera::CameraCapturer;
use crate::pipewire_capture::PipewireCapturer;

/// How long to wait for `PipeWire` to negotiate a format before deciding it will not.
///
/// Negotiation is a round trip with the daemon and the device; a camera that has not
/// settled within this has something wrong with it, and falling back is better than
/// blocking a call's video indefinitely.
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(3);

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

    /// The next frame, if one is waiting.
    #[must_use]
    pub fn try_recv(&self) -> Option<I420Frame> {
        match self {
            Self::Pipewire(c) => c.try_recv(),
            Self::V4l2(c) => c.try_recv(),
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

/// Start the first `PipeWire` source that actually negotiates a format.
///
/// A node can connect and then fail to agree on a format -- a virtual camera offering only
/// layouts we cannot decode does exactly that -- so "connected" is not enough to call it
/// working. Each candidate is given until [`NEGOTIATION_TIMEOUT`] to report a size, and the
/// next is tried otherwise.
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
            Ok(capturer) => {
                let deadline = Instant::now().checked_add(NEGOTIATION_TIMEOUT);
                while deadline.is_some_and(|d| Instant::now() < d) {
                    let (w, h) = capturer.size();
                    if w > 0 && h > 0 {
                        tracing::info!(
                            node_id = source.node_id,
                            name = %source.description,
                            width = w,
                            height = h,
                            "Camera capture started via PipeWire"
                        );
                        return Ok(VideoSource::Pipewire(capturer));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                capturer.stop();
                last_error = format!(
                    "node {} ({}) connected but negotiated no usable format",
                    source.node_id, source.description
                );
                tracing::warn!(reason = %last_error, "Skipping PipeWire source");
            }
            Err(e) => {
                last_error = format!("node {}: {e}", source.node_id);
                tracing::warn!(reason = %last_error, "Skipping PipeWire source");
            }
        }
    }
    Err(last_error)
}
