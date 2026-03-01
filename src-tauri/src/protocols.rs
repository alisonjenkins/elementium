use tauri::http::{Request, Response};
use tauri::{Manager, UriSchemeContext, UriSchemeResponder};

use elementium_webrtc::VideoFrameBuffer;

/// Tauri-managed state holding a direct reference to the shared video frame buffer.
/// Avoids locking the WebRTC engine mutex on every frame fetch.
pub struct VideoFrameState(pub VideoFrameBuffer);

/// Handle requests to `elementium://video-frame/{track-id}`.
///
/// Returns the latest RGBA frame for the given track as raw bytes,
/// with `X-Frame-Width` and `X-Frame-Height` headers.
pub fn handle_video_frame_protocol(
    ctx: UriSchemeContext<'_, tauri::Wry>,
    request: Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    let uri = request.uri().to_string();

    // Parse track ID from URI: elementium://video-frame/{track-id}
    let track_id = uri
        .strip_prefix("elementium://video-frame/")
        .or_else(|| uri.strip_prefix("elementium://localhost/video-frame/"))
        .unwrap_or("");

    if track_id.is_empty() {
        responder.respond(
            Response::builder()
                .status(400)
                .body(b"Missing track ID".to_vec())
                .unwrap(),
        );
        return;
    }

    // Look up the video frame directly from the shared buffer (no engine lock needed)
    let frame = {
        let app = ctx.app_handle();
        let state: tauri::State<'_, VideoFrameState> = app.state();
        state.0.lock().ok().and_then(|f| f.get(track_id).cloned())
    };

    // Log first few requests for debugging
    static FRAME_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = FRAME_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if count < 5 || count % 300 == 0 {
        tracing::info!(
            track_id = %track_id,
            has_frame = frame.is_some(),
            count,
            "Video frame protocol request"
        );
    }

    match frame {
        Some(video_frame) => {
            responder.respond(
                Response::builder()
                    .status(200)
                    .header("Content-Type", "application/octet-stream")
                    .header("X-Frame-Width", video_frame.width.to_string())
                    .header("X-Frame-Height", video_frame.height.to_string())
                    .header("Access-Control-Allow-Origin", "*")
                    .header(
                        "Access-Control-Expose-Headers",
                        "X-Frame-Width, X-Frame-Height",
                    )
                    .body(video_frame.data)
                    .unwrap(),
            );
        }
        None => {
            // No frame available yet — return a 1x1 transparent pixel
            let placeholder = vec![0u8; 4];
            responder.respond(
                Response::builder()
                    .status(200)
                    .header("Content-Type", "application/octet-stream")
                    .header("X-Frame-Width", "1")
                    .header("X-Frame-Height", "1")
                    .header("Access-Control-Allow-Origin", "*")
                    .header(
                        "Access-Control-Expose-Headers",
                        "X-Frame-Width, X-Frame-Height",
                    )
                    .body(placeholder)
                    .unwrap(),
            );
        }
    }
}
