//! Wayland screen capture via the XDG desktop portal and `PipeWire`.
//!
//! There is no way to read the screen directly on Wayland: the compositor does not expose
//! one, by design. The supported route is the `org.freedesktop.portal.ScreenCast` portal,
//! which shows the user a picker and hands back a `PipeWire` node id for whatever they
//! chose. Frames then arrive over `PipeWire` exactly as camera frames do, so this reuses
//! the same capture path rather than growing a second one.
//!
//! The portal call is asynchronous and shows UI; [`ScreenCapturer::start`] is synchronous,
//! so the portal exchange runs on a short-lived runtime and blocks until the user has
//! chosen. That wait is unbounded on purpose -- it is a person deciding, and a timeout
//! would cancel a dialog they were still reading.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use elementium_types::{CaptureSource, ElementiumError, I420Frame};

use crate::traits::ScreenCapturer;

/// Wayland screen capturer.
pub struct WaylandCapturer {
    /// Cleared to stop the pump thread, which owns the capture and closes it on the way out.
    running: Arc<AtomicBool>,
}

impl Default for WaylandCapturer {
    fn default() -> Self {
        Self::new()
    }
}

impl WaylandCapturer {
    #[must_use]
    pub fn new() -> Self {
        Self { running: Arc::new(AtomicBool::new(false)) }
    }
}

/// Ask the portal for a screencast node id.
///
/// Returns the `PipeWire` node id of the first stream the user selected. Multiple streams
/// are possible when several outputs are picked; the first is used, because everything
/// above this expects a single video track.
#[cfg(target_os = "linux")]
async fn request_screencast_node() -> Result<u32, String> {
    use ashpd::desktop::screencast::{
        CursorMode, Screencast, SelectSourcesOptions, SourceType, StartCastOptions,
    };
    use ashpd::desktop::CreateSessionOptions;

    let proxy = Screencast::new().await.map_err(|e| e.to_string())?;
    let session = proxy
        .create_session(CreateSessionOptions::default())
        .await
        .map_err(|e| e.to_string())?;

    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(SourceType::Monitor | SourceType::Window)
                // Single source: the rest of the pipeline publishes one video track.
                .set_multiple(false)
                .set_persist_mode(ashpd::desktop::PersistMode::DoNot),
        )
        .await
        .map_err(|e| e.to_string())?;

    let response = proxy
        .start(&session, None, StartCastOptions::default())
        .await
        .map_err(|e| e.to_string())?
        .response()
        .map_err(|e| e.to_string())?;

    response
        .streams()
        .first()
        .map(ashpd::desktop::screencast::Stream::pipe_wire_node_id)
        .ok_or_else(|| "the portal returned no streams; the picker was probably cancelled".to_owned())
}

impl ScreenCapturer for WaylandCapturer {
    fn sources(&self) -> Result<Vec<CaptureSource>, ElementiumError> {
        // Source selection is the portal's job, not ours: the compositor will not tell an
        // application what windows exist, and showing our own list would either be empty or
        // a lie. An empty list here means "ask the portal", which `start` does.
        Ok(vec![])
    }

    fn start(
        &mut self,
        _source_id: &str,
        callback: Box<dyn Fn(I420Frame) + Send>,
    ) -> Result<(), ElementiumError> {
        tracing::info!("Requesting a screencast session from the XDG desktop portal");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| ElementiumError::ScreenCapture(format!("runtime: {e}")))?;

        let node_id = runtime.block_on(request_screencast_node()).map_err(|e| {
            tracing::warn!(reason = %e, "Screencast portal request failed");
            ElementiumError::ScreenCapture(e)
        })?;

        tracing::info!(node_id, "Portal granted a screencast stream");

        let capture = elementium_media::pipewire_capture::PipewireCapturer::start(node_id)
            .map_err(|e| ElementiumError::ScreenCapture(e.to_string()))?;

        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);

        // Frames are pumped on a thread so `start` returns once capture is live, matching
        // the trait's callback contract.
        std::thread::Builder::new()
            .name("wayland-screencast".to_owned())
            .spawn(move || {
                while running.load(Ordering::SeqCst) {
                    // Screen capture is always pixels: the portal hands back raw buffers,
                    // never MJPEG, so the compressed variant cannot arise here. Decoding
                    // rather than asserting keeps that true even if it ever changes.
                    if let Some(frame) = capture.try_recv().and_then(|f| f.to_planar()) {
                        callback(frame);
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                }
                capture.stop();
                tracing::info!("Wayland screencast pump stopped");
            })
            .map_err(|e| ElementiumError::ScreenCapture(format!("thread: {e}")))?;

        Ok(())
    }

    fn stop(&mut self) -> Result<(), ElementiumError> {
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }
}
