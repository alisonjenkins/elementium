// `get_display_media` below takes a `State<'_, T>` parameter, which causes the
// `#[command]` macro to generate a sibling IPC-dispatch wrapper item in this module
// containing an internal match with an arm clippy flags as unreachable. That wrapper is
// framework codegen (not nested inside the fn item itself, so a function- or
// statement-scoped `#[allow]` cannot reach it — verified empirically), hence the
// module-level allow here rather than the usual per-item scoping.
#![allow(clippy::unreachable)]
use tauri::{State, command};

use elementium_types::{CaptureSource, TrackId};

use super::webrtc::WebRtcState;

#[command]
pub async fn get_capture_sources() -> Result<Vec<CaptureSource>, String> {
    tracing::info!("Getting available capture sources");

    // Use the appropriate platform capturer
    #[cfg(target_os = "linux")]
    {
        use elementium_screen::ScreenCapturer;

        // Try X11 first (works on both X11 and XWayland)
        let capturer = elementium_screen::x11::X11Capturer::new();
        match capturer.sources() {
            Ok(sources) => Ok(sources),
            Err(e) => {
                tracing::warn!("X11 source enumeration failed: {e}, trying Wayland");
                // Fall back to Wayland (returns empty list, portal handles selection)
                let capturer = elementium_screen::wayland::WaylandCapturer::new();
                capturer.sources().map_err(|e| e.to_string())
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        Ok(vec![])
    }
}

#[command]
pub async fn get_display_media(
    _webrtc_state: State<'_, WebRtcState>,
    source_id: String,
) -> Result<TrackId, String> {
    tracing::info!(source_id = %source_id, "Starting screen capture");

    let track_id = TrackId(format!("screen-{}", generate_track_id()));

    // Start screen capture on the selected source
    #[cfg(target_os = "linux")]
    {
        use elementium_screen::ScreenCapturer;

        let (frame_tx, _frame_rx) = std::sync::mpsc::sync_channel(4);

        let mut capturer = elementium_screen::x11::X11Capturer::new();
        capturer
            .start(
                &source_id,
                Box::new(move |frame| {
                    let _ = frame_tx.try_send(frame);
                }),
            )
            .map_err(|e| e.to_string())?;

        // TODO: Wire frame_rx into the video pipeline for encoding and transmission
    }

    Ok(track_id)
}

fn generate_track_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{t:x}")
}
