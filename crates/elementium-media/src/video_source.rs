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

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Receiver;
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

/// A video source fed by a capturer that pushes frames from a thread it owns, rather than
/// one this module pulls from (contrast [`PipewireCapturer`]).
///
/// This crate cannot depend on `elementium-screen` -- the dependency runs the other way, so
/// that crate can use [`VideoSource`] for its own capturers -- so this type knows nothing
/// about X11 or `xcap` specifically. It is the generic half of the bridge; the concrete half,
/// which actually owns an `X11Capturer` and feeds `tx`, lives in the Tauri command layer that
/// depends on both crates. See `crates/elementium-screen/src/x11.rs` for why X11 capture is
/// shaped this way at all.
pub struct PushCapturer {
    rx: Receiver<CapturedFrame>,
    /// Updated from the most recently received frame, since -- unlike `PipeWire` -- nothing
    /// here negotiates a size up front; the first frame is the first anyone learns it.
    width: AtomicU32,
    height: AtomicU32,
    /// Tells the producer thread to stop. Called from [`PushCapturer::stop`] and from
    /// `Drop`, matching every other capturer in this module: a pipeline loop that exits
    /// without calling `stop()` explicitly (an error return, a panic unwind) must not leave
    /// the producer thread running forever, capturing frames nobody will ever read.
    stopper: Box<dyn Fn() + Send + Sync>,
    /// Distinguishes this producer from others sharing the same generic plumbing in logs.
    label: &'static str,
}

impl PushCapturer {
    /// The next pushed frame, if one has arrived since the last call.
    #[must_use]
    pub fn try_recv(&self) -> Option<CapturedFrame> {
        let frame = self.rx.try_recv().ok()?;
        self.width.store(frame.width(), Ordering::Relaxed);
        self.height.store(frame.height(), Ordering::Relaxed);
        Some(frame)
    }

    /// The size of the most recently received frame, or `(0, 0)` before the first one.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.width.load(Ordering::Relaxed), self.height.load(Ordering::Relaxed))
    }

    /// Which producer this is, for logging.
    #[must_use]
    pub const fn backend(&self) -> &'static str {
        self.label
    }

    /// Signal the producer thread to stop.
    ///
    /// Idempotent, like the stop signal on every other capturer here -- calling it twice
    /// (once explicitly, once from `Drop`) must not be an error.
    pub fn stop(&self) {
        (self.stopper)();
    }
}

impl Drop for PushCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A running video capture, from whichever backend worked.
pub enum VideoSource {
    Pipewire(PipewireCapturer),
    V4l2(CameraCapturer),
    /// Fed by a push-based producer -- X11 today, see [`PushCapturer`].
    Push(PushCapturer),
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

    /// Build a video source from frames pushed on `rx`, rather than pulled.
    ///
    /// `stopper` is called both when [`VideoSource::stop`] is called explicitly and as a
    /// `Drop` backstop, and must end the thread pushing into `rx` -- otherwise that thread
    /// outlives this source, still capturing frames on its own schedule with nothing left to
    /// read them, until whatever owns it is torn down some other way. `label` names the
    /// producer in logs (`"x11"` today; see [`PushCapturer`] for why this crate cannot name
    /// it more specifically itself).
    #[must_use]
    pub fn start_push(
        rx: Receiver<CapturedFrame>,
        stopper: Box<dyn Fn() + Send + Sync>,
        label: &'static str,
    ) -> Self {
        Self::Push(PushCapturer {
            rx,
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            stopper,
            label,
        })
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
            Self::Push(c) => c.try_recv(),
        }
    }

    /// Frame size, once known.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        match self {
            Self::Pipewire(c) => c.size(),
            Self::V4l2(c) => (c.width(), c.height()),
            Self::Push(c) => c.size(),
        }
    }

    /// Which backend is in use, for logging.
    #[must_use]
    pub const fn backend(&self) -> &'static str {
        match self {
            Self::Pipewire(_) => "pipewire",
            Self::V4l2(_) => "v4l2",
            Self::Push(c) => c.backend(),
        }
    }

    /// Stop capturing.
    pub fn stop(&self) {
        match self {
            Self::Pipewire(c) => c.stop(),
            Self::V4l2(c) => c.stop(),
            Self::Push(c) => c.stop(),
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

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use elementium_types::I420Frame;

    use super::VideoSource;
    use crate::captured_frame::CapturedFrame;

    fn tiny_frame() -> I420Frame {
        // 2x2 is the smallest size `from_planes` will accept: one Y byte per pixel, one
        // rounded-up-to-1x1 U/V byte each. The pixel values are irrelevant here -- only the
        // frame's identity (its size) is being checked.
        I420Frame::from_planes(2, 2, &[0, 0, 0, 0], &[0], &[0], 0).expect("2x2 planes are valid")
    }

    /// This is the whole point of the push adapter: a frame pushed onto the channel by a
    /// producer thread this test stands in for (X11's, in production) has to come back out
    /// of `try_recv` exactly as sent, with `size()` reflecting it -- proving the pull-shaped
    /// `VideoSource` interface really does read what a push-shaped producer wrote.
    #[test]
    fn a_pushed_frame_is_received_and_sizes_the_source() {
        let (tx, rx) = std::sync::mpsc::channel();
        let source = VideoSource::start_push(rx, Box::new(|| {}), "test-push");

        assert_eq!(source.size(), (0, 0), "no frame has arrived yet");
        assert!(source.try_recv().is_none(), "nothing was pushed yet");

        let _ = tx.send(CapturedFrame::Planar(tiny_frame()));

        let received = source.try_recv();
        assert!(received.is_some(), "the pushed frame must be received");
        if let Some(frame) = received {
            assert_eq!((frame.width(), frame.height()), (2, 2));
        }
        assert_eq!(source.size(), (2, 2), "size must update from the received frame");
    }

    /// The producer thread has to be told to stop, both when `stop()` is called explicitly
    /// and when the source is simply dropped (a pipeline loop exiting on an error path,
    /// say) -- otherwise it keeps running with nothing left reading what it pushes. Both
    /// paths go through the same `stopper` closure, so this pins that it actually runs
    /// rather than being wired up and silently never called.
    #[test]
    fn dropping_or_stopping_a_push_source_signals_its_producer() {
        let (_tx, rx) = std::sync::mpsc::channel::<CapturedFrame>();
        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_writer = Arc::clone(&stopped);

        let source = VideoSource::start_push(
            rx,
            Box::new(move || stopped_writer.store(true, Ordering::Relaxed)),
            "test-push",
        );
        assert!(!stopped.load(Ordering::Relaxed), "not stopped before either signal");

        source.stop();
        assert!(stopped.load(Ordering::Relaxed), "stop() must signal the producer");

        drop(source);
        // stop() is idempotent (see `PushCapturer::stop`'s doc), so Drop calling it again is
        // expected and harmless; the assertion above already proved the signal fires.
    }
}
