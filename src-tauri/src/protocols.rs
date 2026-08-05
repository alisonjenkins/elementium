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
// `ctx` and `request` are only borrowed internally, but the signature is dictated by
// tauri's `register_asynchronous_uri_scheme_protocol`, which requires
// `Fn(UriSchemeContext<'_, R>, Request<Vec<u8>>, UriSchemeResponder)` — the params can't
// be changed to references without breaking that trait bound.
#[allow(clippy::needless_pass_by_value)]
pub fn handle_video_frame_protocol(
    ctx: UriSchemeContext<'_, tauri::Wry>,
    request: Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    // Log first few requests for debugging.
    static FRAME_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let uri = request.uri().to_string();

    // Parse track ID from URI: elementium://video-frame/{track-id}
    let track_id = uri
        .strip_prefix("elementium://video-frame/")
        .or_else(|| uri.strip_prefix("elementium://localhost/video-frame/"))
        .unwrap_or("");

    if track_id.is_empty() {
        // Fixed status/body literals — `Response::builder().body()` cannot fail here.
        #[allow(clippy::unwrap_used)]
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

    let count = FRAME_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if count < 5 || count.is_multiple_of(300) {
        tracing::info!(
            track_id = %track_id,
            has_frame = frame.is_some(),
            count,
            "Video frame protocol request"
        );
    }

    let (width, height, body) = frame.map_or_else(
        // No frame available yet — return a 1x1 transparent pixel.
        || ("1".to_string(), "1".to_string(), vec![0u8; 4]),
        |video_frame| {
            (
                video_frame.width.to_string(),
                video_frame.height.to_string(),
                video_frame.data,
            )
        },
    );

    // Fixed header names and a numeric-string/status-200 body — cannot fail here.
    #[allow(clippy::unwrap_used)]
    responder.respond(
        Response::builder()
            .status(200)
            .header("Content-Type", "application/octet-stream")
            .header("X-Frame-Width", width)
            .header("X-Frame-Height", height)
            .header("Access-Control-Allow-Origin", "*")
            .header(
                "Access-Control-Expose-Headers",
                "X-Frame-Width, X-Frame-Height",
            )
            .body(body)
            .unwrap(),
    );
}
