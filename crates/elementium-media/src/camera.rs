//! Camera capture using nokhwa (V4L2 on Linux, AVFoundation on macOS, MediaFoundation on Windows).

use std::sync::mpsc;

use elementium_types::VideoFrame;

/// Error type for camera operations.
#[derive(Debug, thiserror::Error)]
pub enum CameraError {
    #[error("No camera found")]
    NoCameraFound,
    #[error("Camera error: {0}")]
    Camera(String),
}

/// Captures video frames from a camera device.
///
/// The camera is opened and polled on a background thread.
/// Frames are sent to the main thread via a bounded channel.
pub struct CameraCapturer {
    frame_rx: mpsc::Receiver<VideoFrame>,
    stop_tx: mpsc::Sender<()>,
    width: u32,
    height: u32,
}

impl CameraCapturer {
    /// Start capturing from the default camera (index 0).
    pub fn start() -> Result<Self, CameraError> {
        Self::start_with_index(0)
    }

    /// Start capturing from a specific camera index.
    ///
    /// The camera is opened on a background thread since `nokhwa::Camera` is not `Send`.
    pub fn start_with_index(camera_index: u32) -> Result<Self, CameraError> {
        let (frame_tx, frame_rx) = mpsc::sync_channel::<VideoFrame>(4);
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        // Channel to report initial resolution (or error) back to caller
        let (init_tx, init_rx) = mpsc::channel::<Result<(u32, u32), CameraError>>();

        std::thread::spawn(move || {
            let index = nokhwa::utils::CameraIndex::Index(camera_index);
            let requested =
                nokhwa::utils::RequestedFormat::new::<nokhwa::pixel_format::RgbFormat>(
                    nokhwa::utils::RequestedFormatType::AbsoluteHighestFrameRate,
                );

            let mut camera = match nokhwa::Camera::new(index, requested) {
                Ok(c) => c,
                Err(e) => {
                    let _ = init_tx.send(Err(CameraError::Camera(e.to_string())));
                    return;
                }
            };

            if let Err(e) = camera.open_stream() {
                let _ = init_tx.send(Err(CameraError::Camera(e.to_string())));
                return;
            }

            let resolution = camera.resolution();
            let width = resolution.width_x;
            let height = resolution.height_y;

            tracing::info!(width, height, "Camera capture started");
            let _ = init_tx.send(Ok((width, height)));

            loop {
                // Check for stop signal
                if stop_rx.try_recv().is_ok() {
                    tracing::info!("Camera capture stopping");
                    break;
                }

                match camera.frame() {
                    Ok(buffer) => {
                        let res = buffer.resolution();
                        let w = res.width_x;
                        let h = res.height_y;

                        let raw = buffer.buffer();
                        let pixel_count = (w * h) as usize;
                        let expected_rgb = pixel_count * 3;
                        let expected_yuyv = pixel_count * 2;

                        let rgba = if raw.len() >= expected_rgb {
                            // RGB format (3 bytes per pixel) → RGBA
                            let mut out = Vec::with_capacity(pixel_count * 4);
                            for i in 0..pixel_count {
                                out.push(raw[i * 3]);     // R
                                out.push(raw[i * 3 + 1]); // G
                                out.push(raw[i * 3 + 2]); // B
                                out.push(255);             // A
                            }
                            out
                        } else if raw.len() >= expected_yuyv {
                            // YUYV format (2 bytes per pixel, packed) → RGBA
                            yuyv_to_rgba(w, h, raw)
                        } else {
                            tracing::debug!(
                                buf_len = raw.len(),
                                expected_rgb,
                                expected_yuyv,
                                "Unknown camera buffer format, skipping frame"
                            );
                            std::thread::sleep(std::time::Duration::from_millis(5));
                            continue;
                        };

                        let frame = VideoFrame {
                            width: w,
                            height: h,
                            data: rgba,
                            timestamp_us: 0,
                        };

                        // Non-blocking send; drop frame if buffer full
                        let _ = frame_tx.try_send(frame);
                    }
                    Err(e) => {
                        tracing::debug!("Camera frame error: {e}");
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }
            }
        });

        // Wait for the camera thread to initialize
        let (width, height) = init_rx
            .recv()
            .map_err(|_| CameraError::Camera("Camera thread died during init".into()))??;

        Ok(Self {
            frame_rx,
            stop_tx,
            width,
            height,
        })
    }

    /// Try to get the next frame (non-blocking).
    pub fn try_recv(&self) -> Option<VideoFrame> {
        self.frame_rx.try_recv().ok()
    }

    /// Get the next frame (blocking).
    pub fn recv(&self) -> Option<VideoFrame> {
        self.frame_rx.recv().ok()
    }

    /// Stop the camera capture.
    pub fn stop(&self) {
        let _ = self.stop_tx.send(());
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
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
/// YUYV packs two pixels into 4 bytes: [Y0, U, Y1, V].
/// Each pair shares U and V chroma values.
fn yuyv_to_rgba(width: u32, height: u32, yuyv: &[u8]) -> Vec<u8> {
    let pixel_count = (width * height) as usize;
    let mut rgba = Vec::with_capacity(pixel_count * 4);

    // Process two pixels at a time (4 bytes of YUYV → 8 bytes of RGBA)
    let pair_count = pixel_count / 2;
    for i in 0..pair_count {
        let base = i * 4;
        let y0 = yuyv[base] as f32;
        let u = yuyv[base + 1] as f32 - 128.0;
        let y1 = yuyv[base + 2] as f32;
        let v = yuyv[base + 3] as f32 - 128.0;

        // BT.601 YUV → RGB
        let r0 = (y0 + 1.402 * v).clamp(0.0, 255.0) as u8;
        let g0 = (y0 - 0.344 * u - 0.714 * v).clamp(0.0, 255.0) as u8;
        let b0 = (y0 + 1.772 * u).clamp(0.0, 255.0) as u8;

        let r1 = (y1 + 1.402 * v).clamp(0.0, 255.0) as u8;
        let g1 = (y1 - 0.344 * u - 0.714 * v).clamp(0.0, 255.0) as u8;
        let b1 = (y1 + 1.772 * u).clamp(0.0, 255.0) as u8;

        rgba.push(r0);
        rgba.push(g0);
        rgba.push(b0);
        rgba.push(255);

        rgba.push(r1);
        rgba.push(g1);
        rgba.push(b1);
        rgba.push(255);
    }

    rgba
}
