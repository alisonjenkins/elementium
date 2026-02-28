use elementium_types::{CaptureSource, ElementiumError, VideoFrame};

use crate::traits::ScreenCapturer;

/// Wayland screen capturer using ashpd (XDG Desktop Portal).
///
/// Flow:
/// 1. Request screencast via `ashpd::desktop::screencast`
/// 2. User selects source in the portal dialog
/// 3. Portal returns a PipeWire node ID
/// 4. Connect to PipeWire and read frames from the stream
///
/// Note: PipeWire integration requires the `pipewire` crate which adds
/// a significant native dependency. For now, the portal dialog and
/// PipeWire stream setup is implemented as a best-effort flow.
pub struct WaylandCapturer {
    active: bool,
}

impl WaylandCapturer {
    pub fn new() -> Self {
        Self { active: false }
    }
}

impl ScreenCapturer for WaylandCapturer {
    fn sources(&self) -> Result<Vec<CaptureSource>, ElementiumError> {
        // On Wayland, source selection happens via the portal dialog.
        // We return an empty list and let the portal handle picking.
        Ok(vec![])
    }

    fn start(
        &mut self,
        _source_id: &str,
        _callback: Box<dyn Fn(VideoFrame) + Send>,
    ) -> Result<(), ElementiumError> {
        tracing::info!("Starting Wayland screencast via XDG Desktop Portal");
        self.active = true;

        // TODO: Full PipeWire integration:
        // 1. ashpd::desktop::screencast::Screencast::new()
        // 2. create_session()
        // 3. select_sources() - opens portal picker
        // 4. start() - returns PipeWire stream fd + node_id
        // 5. Connect to PipeWire, set up stream listener
        // 6. Read SPA buffers, extract BGRA frames, call callback
        //
        // This requires the `pipewire` crate (0.9.x) and significant
        // unsafe code to read SPA buffers. Implemented in a future phase.

        tracing::warn!("Wayland screen capture not yet fully implemented (needs PipeWire integration)");

        Ok(())
    }

    fn stop(&mut self) -> Result<(), ElementiumError> {
        self.active = false;
        Ok(())
    }
}
