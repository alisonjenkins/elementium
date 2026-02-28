use tauri::http::{Request, Response};
use tauri::{Manager, UriSchemeContext, UriSchemeResponder};

use crate::commands::webrtc::WebRtcState;

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

    tracing::trace!(track_id = %track_id, "Video frame requested");

    // Look up the video frame buffer from the WebRTC engine
    let frame = {
        let app = ctx.app_handle();
        let state: tauri::State<'_, WebRtcState> = app.state();
        let engine = match state.0.lock() {
            Ok(e) => e,
            Err(_) => {
                responder.respond(
                    Response::builder()
                        .status(500)
                        .body(b"Engine lock failed".to_vec())
                        .unwrap(),
                );
                return;
            }
        };
        let frames = engine.video_frames.lock().ok();
        frames.and_then(|f| f.get(track_id).cloned())
    };

    match frame {
        Some(video_frame) => {
            responder.respond(
                Response::builder()
                    .status(200)
                    .header("Content-Type", "application/octet-stream")
                    .header("X-Frame-Width", video_frame.width.to_string())
                    .header("X-Frame-Height", video_frame.height.to_string())
                    .header("Access-Control-Allow-Origin", "*")
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
                    .body(placeholder)
                    .unwrap(),
            );
        }
    }
}
