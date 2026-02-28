use tauri::http::{Request, Response};
use tauri::{UriSchemeContext, UriSchemeResponder};

/// Handle requests to `elementium://video-frame/{track-id}`.
///
/// Returns the latest RGBA frame for the given track as raw bytes,
/// with `X-Frame-Width` and `X-Frame-Height` headers.
pub fn handle_video_frame_protocol(
    _ctx: UriSchemeContext<'_, tauri::Wry>,
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

    // TODO: Look up the latest frame from the ring buffer for this track
    // For now, return a 1x1 transparent RGBA pixel
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
