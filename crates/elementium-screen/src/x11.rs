use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use elementium_types::{CaptureSource, CaptureSourceKind, ElementiumError, VideoFrame};

use crate::traits::ScreenCapturer;

/// X11 screen capturer using xcap.
pub struct X11Capturer {
    active: Arc<AtomicBool>,
}

impl X11Capturer {
    pub fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl ScreenCapturer for X11Capturer {
    fn sources(&self) -> Result<Vec<CaptureSource>, ElementiumError> {
        let mut sources = Vec::new();

        // Enumerate monitors
        if let Ok(monitors) = xcap::Monitor::all() {
            for monitor in monitors {
                let id = monitor.id().unwrap_or(0);
                let name = monitor.name().unwrap_or_default();
                sources.push(CaptureSource {
                    id: format!("monitor-{id}"),
                    name,
                    kind: CaptureSourceKind::Monitor,
                    thumbnail: None,
                });
            }
        }

        // Enumerate windows
        if let Ok(windows) = xcap::Window::all() {
            for window in windows {
                if window.is_minimized().unwrap_or(false) {
                    continue;
                }
                let id = window.id().unwrap_or(0);
                let title = window.title().unwrap_or_default();
                if title.is_empty() {
                    continue;
                }
                sources.push(CaptureSource {
                    id: format!("window-{id}"),
                    name: title,
                    kind: CaptureSourceKind::Window,
                    thumbnail: None,
                });
            }
        }

        Ok(sources)
    }

    fn start(
        &mut self,
        source_id: &str,
        callback: Box<dyn Fn(VideoFrame) + Send>,
    ) -> Result<(), ElementiumError> {
        tracing::info!(source_id = %source_id, "Starting X11 capture");
        self.active.store(true, Ordering::Relaxed);

        let active = self.active.clone();
        let source_id = source_id.to_string();

        std::thread::spawn(move || {
            // Parse the source ID to find the target
            let (kind, id) = if let Some(id_str) = source_id.strip_prefix("monitor-") {
                ("monitor", id_str.parse::<u32>().unwrap_or(0))
            } else if let Some(id_str) = source_id.strip_prefix("window-") {
                ("window", id_str.parse::<u32>().unwrap_or(0))
            } else {
                tracing::error!("Invalid source ID: {source_id}");
                return;
            };

            // Target frame interval (~30fps)
            let frame_interval = std::time::Duration::from_millis(33);

            while active.load(Ordering::Relaxed) {
                let start = std::time::Instant::now();

                let capture_result = match kind {
                    "monitor" => capture_monitor(id),
                    "window" => capture_window(id),
                    _ => None,
                };

                if let Some(frame) = capture_result {
                    callback(frame);
                }

                // Sleep to maintain target frame rate
                let elapsed = start.elapsed();
                if elapsed < frame_interval {
                    std::thread::sleep(frame_interval - elapsed);
                }
            }

            tracing::info!("X11 capture stopped");
        });

        Ok(())
    }

    fn stop(&mut self) -> Result<(), ElementiumError> {
        self.active.store(false, Ordering::Relaxed);
        Ok(())
    }
}

/// Capture a single frame from a monitor by ID.
fn capture_monitor(target_id: u32) -> Option<VideoFrame> {
    let monitors = xcap::Monitor::all().ok()?;
    let monitor = monitors
        .into_iter()
        .find(|m| m.id().unwrap_or(0) == target_id)?;

    let image = monitor.capture_image().ok()?;
    let width = image.width();
    let height = image.height();

    // xcap returns BGRA data
    let bgra = image.into_raw();

    Some(VideoFrame {
        width,
        height,
        data: bgra,
        timestamp_us: 0,
    })
}

/// Capture a single frame from a window by ID.
fn capture_window(target_id: u32) -> Option<VideoFrame> {
    let windows = xcap::Window::all().ok()?;
    let window = windows
        .into_iter()
        .find(|w| w.id().unwrap_or(0) == target_id)?;

    let image = window.capture_image().ok()?;
    let width = image.width();
    let height = image.height();

    // xcap returns BGRA data
    let bgra = image.into_raw();

    Some(VideoFrame {
        width,
        height,
        data: bgra,
        timestamp_us: 0,
    })
}
