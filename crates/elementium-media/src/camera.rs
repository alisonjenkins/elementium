//! Camera capture using `nokhwa` (`V4L2` on Linux, `AVFoundation` on macOS, `MediaFoundation` on Windows).

use std::sync::mpsc;

use elementium_types::I420Frame;

/// Error type for camera operations.
///
/// Split from a single `Camera(String)` catch-all: enumeration, opening a device, and
/// starting its stream are three different `nokhwa` calls with three different remedies (no
/// hardware at all vs. a device present but rejecting the requested format vs. a device that
/// opened but would not start), and each now carries the underlying [`nokhwa::NokhwaError`]
/// as its source instead of only that error's rendered message.
#[derive(Debug, thiserror::Error)]
pub enum CameraError {
    #[error("No camera found")]
    NoCameraFound,
    #[error("failed to enumerate cameras: {0}")]
    Enumerate(#[source] nokhwa::NokhwaError),
    #[error("failed to open camera: {0}")]
    Open(#[source] nokhwa::NokhwaError),
    #[error("failed to start camera stream: {0}")]
    OpenStream(#[source] nokhwa::NokhwaError),
    /// The background thread that owns the (non-`Send`) `nokhwa::Camera` exited before it
    /// reported whether initialization succeeded -- e.g. it panicked. Not a `nokhwa` failure:
    /// there is no `NokhwaError` to wrap, only the fact that the initialization channel
    /// closed with nothing sent.
    #[error("camera capture thread died during initialization")]
    ThreadDied,
}

/// Captures video frames from a camera device.
///
/// The camera is opened and polled on a background thread.
/// Frames are sent to the main thread via a bounded channel.
pub struct CameraCapturer {
    frame_rx: mpsc::Receiver<I420Frame>,
    stop_tx: mpsc::Sender<()>,
    width: u32,
    height: u32,
}

/// Tracks whether the detected camera buffer format has already been logged,
/// to avoid spamming logs on every frame.
static LOGGED_FORMAT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// List the real device indices of every camera nokhwa's enumeration finds.
///
/// `nokhwa`'s Linux backend opens `CameraIndex::Index(n)` as `/dev/video{n}` directly, so the
/// index must come from actually querying which devices exist rather than assuming `0` --
/// device nodes are not guaranteed to start at 0 (e.g. a system with only `/dev/video2` and up).
/// Enumeration can also include non-capture nodes (e.g. a UVC camera's separate metadata
/// interface, which has zero supported formats) -- callers should try each in order and fall
/// through to the next on failure rather than assuming the first entry is a working camera.
fn candidate_camera_indices() -> Result<Vec<u32>, CameraError> {
    let cameras = nokhwa::query(nokhwa::utils::ApiBackend::Auto).map_err(CameraError::Enumerate)?;
    if cameras.is_empty() {
        return Err(CameraError::NoCameraFound);
    }
    Ok(cameras
        .iter()
        .filter_map(|c| c.index().as_index().ok())
        .collect())
}

/// Build the requested camera format for the given optional resolution.
fn requested_format(
    width: Option<u32>,
    height: Option<u32>,
) -> nokhwa::utils::RequestedFormat<'static> {
    if let (Some(w), Some(h)) = (width, height) {
        nokhwa::utils::RequestedFormat::new::<nokhwa::pixel_format::RgbFormat>(
            nokhwa::utils::RequestedFormatType::Closest(nokhwa::utils::CameraFormat::new(
                nokhwa::utils::Resolution::new(w, h),
                nokhwa::utils::FrameFormat::MJPEG,
                30,
            )),
        )
    } else {
        nokhwa::utils::RequestedFormat::new::<nokhwa::pixel_format::RgbFormat>(
            nokhwa::utils::RequestedFormatType::AbsoluteHighestFrameRate,
        )
    }
}

/// Decode a raw camera frame buffer into RGBA pixel data.
///
/// Returns `None` when the frame should be skipped (undecodable JPEG or an
/// unrecognized buffer layout).
fn decode_frame_to_rgba(w: u32, h: u32, raw: &[u8]) -> Option<Vec<u8>> {
    let pixel_count = usize::try_from(w)
        .ok()?
        .checked_mul(usize::try_from(h).ok()?)?;
    let expected_rgba = pixel_count.checked_mul(4)?;
    let expected_rgb = pixel_count.checked_mul(3)?;
    let expected_yuyv = pixel_count.checked_mul(2)?;

    // Log format on first frame for debugging
    if !LOGGED_FORMAT.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::info!(
            buf_len = raw.len(),
            expected_rgba,
            expected_rgb,
            expected_yuyv,
            first_bytes = ?raw.get(..raw.len().min(16)),
            "Camera buffer format detected"
        );
    }

    if raw.len() >= 2 && raw.first() == Some(&0xFF) && raw.get(1) == Some(&0xD8) {
        // MJPEG frame — decode JPEG to RGBA via libjpeg-turbo
        match turbojpeg::decompress(raw, turbojpeg::PixelFormat::RGBA) {
            Ok(image) => Some(image.pixels),
            Err(e) => {
                tracing::debug!("JPEG decode error: {e}");
                None
            }
        }
    } else if raw.len() == expected_rgba {
        // BGRA or RGBA format (4 bytes per pixel)
        decode_bgra(pixel_count, raw)
    } else if raw.len() == expected_rgb {
        // RGB format (3 bytes per pixel) → RGBA
        decode_rgb(pixel_count, raw)
    } else if raw.len() >= expected_yuyv {
        // YUYV format (2 bytes per pixel, packed) → RGBA
        Some(yuyv_to_rgba(w, h, raw))
    } else {
        tracing::debug!(
            buf_len = raw.len(),
            expected_rgba,
            expected_rgb,
            expected_yuyv,
            "Unknown camera buffer format, skipping frame"
        );
        None
    }
}

/// Convert a BGRA (4 bytes per pixel) buffer into RGBA.
fn decode_bgra(pixel_count: usize, raw: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(pixel_count.checked_mul(4)?);
    for i in 0..pixel_count {
        let base = i.checked_mul(4)?;
        out.push(*raw.get(base.checked_add(2)?)?); // R (was B in BGRA)
        out.push(*raw.get(base.checked_add(1)?)?); // G
        out.push(*raw.get(base)?); // B (was R in BGRA)
        out.push(255); // A
    }
    Some(out)
}

/// Convert an RGB (3 bytes per pixel) buffer into RGBA.
fn decode_rgb(pixel_count: usize, raw: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(pixel_count.checked_mul(4)?);
    for i in 0..pixel_count {
        let base = i.checked_mul(3)?;
        out.push(*raw.get(base)?); // R
        out.push(*raw.get(base.checked_add(1)?)?); // G
        out.push(*raw.get(base.checked_add(2)?)?); // B
        out.push(255); // A
    }
    Some(out)
}

/// Poll the camera in a loop, sending decoded frames until a stop signal is received.
fn run_capture_loop(
    device_id: &str,
    mut camera: nokhwa::Camera,
    frame_tx: &mpsc::SyncSender<I420Frame>,
    stop_rx: &mpsc::Receiver<()>,
) {
    // Undecodable frames since this capture started, so a camera that produces them
    // constantly is distinguishable from one that hiccups once.
    let mut undecodable: u64 = 0;
    loop {
        // Check for stop signal
        if stop_rx.try_recv().is_ok() {
            tracing::info!(device_id = %device_id, "Camera capture stopping");
            break;
        }

        match camera.frame() {
            Ok(buffer) => {
                let res = buffer.resolution();
                let w = res.width_x;
                let h = res.height_y;
                let raw = buffer.buffer();

                let Some(rgba) = decode_frame_to_rgba(w, h, raw) else {
                    // Counted and reported, unlike before. A camera whose frames all fail
                    // to decode looks exactly like a camera producing nothing: the picture
                    // freezes on its last good frame and this loop spins at 5ms forever.
                    // The sibling `Err` branch below has always logged; this one did not,
                    // so the more confusing of the two failures was the silent one.
                    undecodable = undecodable.saturating_add(1);
                    if undecodable == 1 || undecodable.is_multiple_of(100) {
                        tracing::warn!(
                            undecodable,
                            width = w,
                            height = h,
                            bytes = raw.len(),
                            "camera frame could not be decoded; the picture will not advance"
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                };

                // Converted here rather than downstream: capture's contract is planar
                // YUV, which is what every video encoder takes and what the MJPEG path
                // produces without any conversion at all.
                let frame = elementium_codec::rgba_to_i420(w, h, &rgba);

                // Non-blocking send; drop frame if buffer full
                let _ = frame_tx.try_send(frame);
            }
            Err(e) => {
                tracing::debug!(
                    device_id = %device_id,
                    error_kind = "frame_read",
                    error = %e,
                    "Camera frame read error"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

impl CameraCapturer {
    /// Start capturing from the default camera (index 0) at a given resolution.
    ///
    /// # Errors
    ///
    /// Returns [`CameraError`] if no camera is found or the camera cannot be
    /// opened/started.
    pub fn start(width: Option<u32>, height: Option<u32>) -> Result<Self, CameraError> {
        let indices = candidate_camera_indices()?;
        let mut last_err = CameraError::NoCameraFound;
        for index in indices {
            match Self::start_with_index(index, width, height) {
                Ok(capturer) => return Ok(capturer),
                Err(e) => {
                    tracing::warn!(
                        camera_index = index,
                        error = %e,
                        "Camera candidate failed to open, trying next"
                    );
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    /// Start capturing from a specific camera index.
    ///
    /// The camera is opened on a background thread since `nokhwa::Camera` is not `Send`.
    ///
    /// # Errors
    ///
    /// Returns [`CameraError`] if the camera thread dies during initialization
    /// or the camera cannot be opened/started.
    pub fn start_with_index(
        camera_index: u32,
        width: Option<u32>,
        height: Option<u32>,
    ) -> Result<Self, CameraError> {
        let (frame_tx, frame_rx) = mpsc::sync_channel::<I420Frame>(4);
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        // Channel to report initial resolution (or error) back to caller
        let (init_tx, init_rx) = mpsc::channel::<Result<(u32, u32), CameraError>>();

        std::thread::spawn(move || {
            let index = nokhwa::utils::CameraIndex::Index(camera_index);
            let device_id = format!("camera-{camera_index}");
            let requested = requested_format(width, height);

            let mut camera = match nokhwa::Camera::new(index, requested) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(
                        device_id = %device_id,
                        requested_width = width,
                        requested_height = height,
                        error_kind = "open",
                        error = %e,
                        "Failed to open camera device"
                    );
                    let _ = init_tx.send(Err(CameraError::Open(e)));
                    return;
                }
            };

            if let Err(e) = camera.open_stream() {
                tracing::error!(
                    device_id = %device_id,
                    requested_width = width,
                    requested_height = height,
                    error_kind = "open_stream",
                    error = %e,
                    "Failed to open camera stream"
                );
                let _ = init_tx.send(Err(CameraError::OpenStream(e)));
                return;
            }

            let resolution = camera.resolution();
            let actual_width = resolution.width_x;
            let actual_height = resolution.height_y;

            tracing::info!(
                device_id = %device_id,
                requested_width = width,
                requested_height = height,
                width = actual_width,
                height = actual_height,
                "Camera capture started"
            );
            let _ = init_tx.send(Ok((actual_width, actual_height)));

            run_capture_loop(&device_id, camera, &frame_tx, &stop_rx);
        });

        // Wait for the camera thread to initialize
        let (width, height) = init_rx.recv().map_err(|_| {
            tracing::error!(
                camera_index,
                error_kind = "thread_died",
                "Camera thread died during initialization"
            );
            CameraError::ThreadDied
        })??;

        Ok(Self {
            frame_rx,
            stop_tx,
            width,
            height,
        })
    }

    /// Try to get the next frame (non-blocking).
    #[must_use]
    pub fn try_recv(&self) -> Option<I420Frame> {
        self.frame_rx.try_recv().ok()
    }

    /// Get the next frame (blocking).
    #[must_use]
    pub fn recv(&self) -> Option<I420Frame> {
        self.frame_rx.recv().ok()
    }

    /// Stop the camera capture.
    pub fn stop(&self) {
        let _ = self.stop_tx.send(());
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }
}

impl Drop for CameraCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Convert YUYV (YUY2) packed data to RGBA.
///
/// YUYV packs two pixels into 4 bytes: [Y0, U, Y1, V]. Each pair shares U and V chroma
/// values. Uses the `yuv` crate's SIMD-accelerated (AVX2/SSE/NEON, with scalar fallback)
/// converter -- BT.601 full-range, matching this function's previous hand-rolled math.
fn yuyv_to_rgba(width: u32, height: u32, yuyv: &[u8]) -> Vec<u8> {
    let Some(pixel_count) = usize::try_from(width)
        .ok()
        .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
    else {
        return Vec::new();
    };
    let Some(capacity) = pixel_count.checked_mul(4) else {
        return Vec::new();
    };
    let mut rgba = vec![0u8; capacity];

    let packed = yuv::YuvPackedImage {
        yuy: yuyv,
        yuy_stride: width.saturating_mul(2),
        width,
        height,
    };
    let rgba_stride = width.saturating_mul(4);

    if let Err(e) = yuv::yuyv422_to_rgba(
        &packed,
        &mut rgba,
        rgba_stride,
        yuv::YuvRange::Full,
        yuv::YuvStandardMatrix::Bt601,
    ) {
        tracing::error!(width, height, error = %e, "SIMD YUYV->RGBA conversion failed, returning blank frame");
        rgba.fill(0);
    }

    rgba
}

#[cfg(test)]
mod error_tests {
    use super::CameraError;

    /// Pins the split of the old single `Camera(String)` catch-all: opening a device must
    /// report `Open` and must keep the underlying `nokhwa` error walkable via `source()`,
    /// not just folded into a rendered message.
    #[test]
    fn open_failure_carries_the_nokhwa_error_as_its_source() {
        let inner = nokhwa::NokhwaError::OpenDeviceError("cam0".into(), "busy".into());
        let err = CameraError::Open(inner);
        assert!(matches!(err, CameraError::Open(_)));
        assert!(
            std::error::Error::source(&err).is_some(),
            "Open must preserve its nokhwa cause via #[source], not swallow it"
        );
    }

    /// The other half of the same split: starting the stream is a different `nokhwa` call
    /// than opening the device, and must be distinguishable from `Open`.
    #[test]
    fn open_stream_failure_carries_the_nokhwa_error_as_its_source() {
        let inner = nokhwa::NokhwaError::OpenStreamError("format rejected".into());
        let err = CameraError::OpenStream(inner);
        assert!(matches!(err, CameraError::OpenStream(_)));
        assert!(std::error::Error::source(&err).is_some());
    }

    /// Enumeration failing (no backend, permission denied, etc.) is a third distinct `nokhwa`
    /// call and must not be folded into `Open`/`OpenStream`.
    #[test]
    fn enumerate_failure_carries_the_nokhwa_error_as_its_source() {
        let inner = nokhwa::NokhwaError::GeneralError("no backend".into());
        let err = CameraError::Enumerate(inner);
        assert!(matches!(err, CameraError::Enumerate(_)));
        assert!(std::error::Error::source(&err).is_some());
    }

    /// The background thread dying before it reports success/failure has no `nokhwa` error to
    /// wrap at all -- it must be its own variant, not a stringified `Camera(String)`.
    #[test]
    fn thread_died_has_no_underlying_source() {
        let err = CameraError::ThreadDied;
        assert!(matches!(err, CameraError::ThreadDied));
        assert!(std::error::Error::source(&err).is_none());
    }
}
