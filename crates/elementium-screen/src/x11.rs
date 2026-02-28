use elementium_types::{CaptureSource, CaptureSourceKind, ElementiumError, VideoFrame};

use crate::traits::ScreenCapturer;

/// X11 screen capturer using xcap.
pub struct X11Capturer {
    active: bool,
}

impl X11Capturer {
    pub fn new() -> Self {
        Self { active: false }
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
        _callback: Box<dyn Fn(VideoFrame) + Send>,
    ) -> Result<(), ElementiumError> {
        tracing::info!(source_id = %source_id, "Starting X11 capture");
        self.active = true;
        // TODO: Spawn capture loop using xcap::Monitor::capture_image or xcap::Window::capture_image
        Ok(())
    }

    fn stop(&mut self) -> Result<(), ElementiumError> {
        self.active = false;
        Ok(())
    }
}
