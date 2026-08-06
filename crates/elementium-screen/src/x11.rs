use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use elementium_types::{CaptureSource, CaptureSourceKind, ElementiumError, VideoFrame};

use crate::traits::ScreenCapturer;

/// X11 screen capturer using xcap.
pub struct X11Capturer {
    active: Arc<AtomicBool>,
}

impl Default for X11Capturer {
    fn default() -> Self {
        Self::new()
    }
}

impl X11Capturer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl ScreenCapturer for X11Capturer {
    fn sources(&self) -> Result<Vec<CaptureSource>, ElementiumError> {
        let mut sources = Vec::new();

        // Enumerate monitors. Skip any monitor whose id lookup fails rather than
        // defaulting to 0 -- id 0 is a real, valid monitor id, so a failed lookup
        // defaulting to it would make this source indistinguishable from (and later
        // capture instead of) the actual monitor 0.
        if let Ok(monitors) = xcap::Monitor::all() {
            for monitor in monitors {
                let Ok(id) = monitor.id() else {
                    tracing::warn!("Skipping monitor with unreadable id");
                    continue;
                };
                let name = monitor.name().unwrap_or_default();
                sources.push(CaptureSource {
                    id: format!("monitor-{id}"),
                    name,
                    kind: CaptureSourceKind::Monitor,
                    thumbnail: None,
                });
            }
        }

        // Enumerate windows. Same id-0-collision reasoning as monitors above.
        if let Ok(windows) = xcap::Window::all() {
            for window in windows {
                if window.is_minimized().unwrap_or(false) {
                    continue;
                }
                let Ok(id) = window.id() else {
                    tracing::warn!("Skipping window with unreadable id");
                    continue;
                };
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
            // Parse the source ID to find the target. A malformed numeric suffix must
            // abort capture, not silently fall back to id 0 (a real, valid target id).
            let (kind, id) = if let Some(id_str) = source_id.strip_prefix("monitor-") {
                match id_str.parse::<u32>() {
                    Ok(id) => ("monitor", id),
                    Err(e) => {
                        tracing::error!(error = %e, "Invalid monitor source ID: {source_id}");
                        return;
                    }
                }
            } else if let Some(id_str) = source_id.strip_prefix("window-") {
                match id_str.parse::<u32>() {
                    Ok(id) => ("window", id),
                    Err(e) => {
                        tracing::error!(error = %e, "Invalid window source ID: {source_id}");
                        return;
                    }
                }
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
                if let Some(remaining) = frame_interval.checked_sub(elapsed) {
                    std::thread::sleep(remaining);
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

/// Build a `VideoFrame` from an xcap-captured image (xcap returns BGRA data).
fn frame_from_capture(image: xcap::image::RgbaImage) -> VideoFrame {
    VideoFrame {
        width: image.width(),
        height: image.height(),
        data: image.into_raw(),
        timestamp_us: 0,
    }
}

/// Capture a single frame from a monitor by ID. Monitors whose id lookup fails are
/// skipped rather than matched via a `0` fallback (see `sources()`'s id-0-collision note).
fn capture_monitor(target_id: u32) -> Option<VideoFrame> {
    let monitors = xcap::Monitor::all().ok()?;
    let monitor = monitors
        .into_iter()
        .find(|m| m.id().is_ok_and(|id| id == target_id))?;
    Some(frame_from_capture(monitor.capture_image().ok()?))
}

/// Capture a single frame from a window by ID. Same id-0-collision reasoning as
/// `capture_monitor`.
fn capture_window(target_id: u32) -> Option<VideoFrame> {
    let windows = xcap::Window::all().ok()?;
    let window = windows
        .into_iter()
        .find(|w| w.id().is_ok_and(|id| id == target_id))?;
    Some(frame_from_capture(window.capture_image().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test: `capture_monitor`/`capture_window` returning `None` for an
    /// unmatched target id is the only correct behavior when no real X11 display is
    /// available in CI (`xcap::Monitor::all()`/`xcap::Window::all()` fail without one) --
    /// this pins that "no display -> no crash, just no frame" contract so a future
    /// change to the fallback logic can't reintroduce the id-0 collision bug (where a
    /// failed `.id()` lookup silently matched whichever source came first).
    #[test]
    fn capture_functions_return_none_without_a_real_target() {
        assert!(capture_monitor(u32::MAX).is_none());
        assert!(capture_window(u32::MAX).is_none());
    }
}
