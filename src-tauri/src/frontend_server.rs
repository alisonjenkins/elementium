//! Serves the embedded frontend over loopback HTTP.
//!
//! A release build used to load the frontend from Tauri's `tauri://localhost` origin, and
//! from there logging in was impossible. Authentication is a redirect chain that has to
//! come back to us, and `WebKitGTK` refuses to follow a redirect whose target is not
//! HTTP(S) — the homeserver's hop back died on "Redirection to URL with a scheme that is
//! not HTTP(S)". OIDC failed earlier still, because dynamic client registration will not
//! accept a `tauri:` redirect URI.
//!
//! `tauri-plugin-localhost` does this job, but it never answers a request for an asset it
//! does not have: the request is dropped and the socket left open. Element Web probes for
//! a host-specific `config.<hostname>.json` before falling back to `config.json`, so on
//! that plugin the app would hang at startup on a file that is *meant* to be absent. A
//! 404 is not an edge case here, it is part of the normal boot, so this serves the assets
//! itself.
//!
//! Answering that missing asset correctly takes one more step than it looks like it
//! should. Tauri's own resolver never reports a miss: anything it cannot find falls back
//! to `index.html`, so `config.localhost.json` came back `200 text/html`. That is the
//! right behaviour for an app with server-side routes and the wrong one here — Element
//! Web routes on the fragment, so every path that is not an asset is a genuine miss, and
//! a page of HTML delivered in place of JSON or wasm is a confusing failure much later.
//! The fallback is detected by recognising `index.html`'s own bytes coming back under
//! another name.

use std::io::Cursor;

use tauri::{AppHandle, Manager, Runtime};
use tiny_http::{Header, Response, Server};
use tracing::{debug, error, info};

/// Everything Element Web needs is same-origin, and the window is the only client, so the
/// body of a missing asset is only ever read by a developer with the log open.
const NOT_FOUND_BODY: &[u8] = b"not found";

/// The path the entry point is served from, and the one path allowed to return its bytes.
const INDEX_PATH: &str = "/index.html";

/// Why [`spawn`] could not start serving the frontend.
///
/// A named type carrying its real source, per Principle I -- this used to be a
/// `format!(...)`-built `String`, which loses whatever `tiny_http`/`std::io` said as soon as
/// it is built, and gives the caller nothing to match on. `main.rs` still has to flatten this
/// to `String` to satisfy `tauri`'s `setup` closure, but that flattening now goes through one
/// place ([`std::fmt::Display`], via `SetupError` below) that also logs the chain, rather
/// than losing it at the point this error is created.
#[derive(Debug)]
pub enum FrontendServerError {
    /// The loopback port could not be bound -- most commonly because something else (an
    /// earlier, still-running instance of this app; a leftover `--port` override colliding
    /// with the default) already holds it.
    Bind {
        port: u16,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The background thread that serves requests could not be spawned.
    ThreadSpawn(std::io::Error),
}

impl std::fmt::Display for FrontendServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bind { port, .. } => {
                write!(f, "could not serve the frontend on 127.0.0.1:{port}")
            }
            Self::ThreadSpawn(_) => write!(f, "could not start the frontend HTTP thread"),
        }
    }
}

impl std::error::Error for FrontendServerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Bind { source, .. } => Some(source.as_ref()),
            Self::ThreadSpawn(source) => Some(source),
        }
    }
}

/// Start serving `app`'s embedded assets on `127.0.0.1:port`.
///
/// Binds before returning so that a port already in use is an error the caller can report,
/// rather than a blank window. Serving itself continues on a background thread for the
/// lifetime of the process.
///
/// # Errors
///
/// Returns [`FrontendServerError::Bind`] if the port cannot be bound, or
/// [`FrontendServerError::ThreadSpawn`] if the OS refuses to start the serving thread.
pub fn spawn<R: Runtime>(app: &AppHandle<R>, port: u16) -> Result<(), FrontendServerError> {
    // Loopback only. The frontend carries an access token and holds the IPC grant, and
    // there is no reason for anything off this machine to reach either.
    let server = Server::http((std::net::Ipv4Addr::LOCALHOST, port))
        .map_err(|source| FrontendServerError::Bind { port, source })?;

    let resolver = app.asset_resolver();
    // Frames are served from here rather than over Tauri's IPC, and the reason is measured.
    // The frontend is loaded over this loopback HTTP server, so the webview's origin is not
    // a `tauri://` one and Tauri's binary custom-protocol IPC is unavailable -- every log
    // this project has ever produced opens with "IPC custom protocol failed, Tauri will now
    // use the postMessage interface instead". Over postMessage a `Vec<u8>` is serialised as
    // a JSON array of numbers, so one 1280x720 RGBA frame -- 3.7MB -- crosses as roughly
    // three and a half million comma-separated integers. Four canvases at 30fps is
    // impossible by two orders of magnitude, and what was measured was 14fps a track.
    //
    // Here the bytes are bytes: the page `fetch`es an `ArrayBuffer` from the same origin it
    // was itself served from, so there is no serialisation at all.
    let frame_app = app.clone();
    // Read once, so the per-request comparison is against a value that cannot change.
    let index_bytes = resolver.get(INDEX_PATH.to_owned()).map(|a| a.bytes);
    if index_bytes.is_none() {
        // Not fatal here -- the window will fail visibly on its own -- but the reason is
        // worth recording, because "the app is blank" is otherwise unattributable.
        error!("the frontend has no index.html embedded in it");
    }

    std::thread::Builder::new()
        .name("frontend-http".into())
        .spawn(move || {
            info!(port, "serving the embedded frontend over loopback HTTP");
            for request in server.incoming_requests() {
                let path = asset_path(request.url());

                if let Some(track_id) = stream_track_id(&path) {
                    // Answered on its own thread. This loop serves requests one at a time,
                    // and a stream response stays open for the life of the track -- holding
                    // it here would stall every asset and every other track behind it.
                    let streams = frame_app
                        .try_state::<crate::encoded_streams::EncodedStreams>()
                        .map(|s| (*s).clone());
                    if let Some(streams) = streams {
                        spawn_stream_responder(request, track_id, streams);
                    } else {
                        let response = Response::from_data(NOT_FOUND_BODY).with_status_code(503);
                        if let Err(e) = request.respond(response) {
                            debug!(error = %e, "could not refuse a stream request");
                        }
                    }
                    continue;
                }

                if let Some(track_id) = frame_track_id(&path) {
                    let response = frame_response(&frame_app, &track_id);
                    if let Err(e) = request.respond(response) {
                        debug!(error = %e, "the frontend closed a frame request before it was answered");
                    }
                    continue;
                }

                let found = resolver.get(path.clone()).filter(|asset| {
                    // Tauri's resolver answers a miss with index.html; see the module
                    // comment. Only the entry point may legitimately be those bytes.
                    path == INDEX_PATH || index_bytes.as_ref() != Some(&asset.bytes)
                });
                let response = found.map_or_else(
                    || {
                        // Expected during a normal boot, not a fault -- see the module
                        // comment on `config.<hostname>.json`.
                        debug!(%path, "no such embedded asset");
                        Response::from_data(NOT_FOUND_BODY).with_status_code(404)
                    },
                    |asset| {
                        let mut response = Response::new(
                            200.into(),
                            Vec::new(),
                            Cursor::new(asset.bytes),
                            None,
                            None,
                        );
                        add_header(&mut response, "Content-Type", &asset.mime_type);
                        if let Some(csp) = asset.csp_header {
                            add_header(&mut response, "Content-Security-Policy", &csp);
                        }
                        // The assets are baked into the binary, so the only thing a cache
                        // could do is serve the previous build after an update.
                        add_header(&mut response, "Cache-Control", "no-cache");
                        response
                    },
                );
                if let Err(e) = request.respond(response) {
                    debug!(error = %e, "the frontend closed a connection before it was answered");
                }
            }
            error!("the frontend HTTP server stopped accepting requests");
        })
        .map_err(FrontendServerError::ThreadSpawn)?;

    Ok(())
}

/// The prefix a track's encoded-frame stream is served under.
const STREAM_PREFIX: &str = "/__elementium/stream/";

/// The track id a request is asking to stream, if it is asking to stream one.
fn stream_track_id(path: &str) -> Option<String> {
    let raw = path.strip_prefix(STREAM_PREFIX)?;
    if raw.is_empty() {
        return None;
    }
    Some(percent_decode(raw))
}

/// How long the reader waits for the next frame before checking whether it should still be
/// here. Short enough that a closed track is noticed promptly, long enough not to spin.
const STREAM_IDLE_POLL: std::time::Duration = std::time::Duration::from_millis(5);

/// How long a stream with no frames at all is kept open.
///
/// A track that has produced nothing for this long is over -- the participant left, or the
/// connection was replaced -- and the response must end so the thread does not outlive it.
/// Generously longer than the gap between keyframes, so a quiet camera is not mistaken for
/// a dead one.
const STREAM_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A `Read` over a track's encoded frames, for `tiny_http` to write out as it goes.
///
/// Blocking rather than returning `Ok(0)` on an empty queue: zero from `read` means end of
/// stream, and a live track between frames has not ended.
struct FrameStreamReader {
    streams: crate::encoded_streams::EncodedStreams,
    track_id: String,
    /// The remainder of a frame too large for the last `read` buffer.
    pending: std::io::Cursor<Vec<u8>>,
    idle_since: std::time::Instant,
}

impl std::io::Read for FrameStreamReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let taken = self.pending.read(buf)?;
            if taken > 0 {
                return Ok(taken);
            }
            if let Some(frame) = self.streams.pop(&self.track_id) {
                self.idle_since = std::time::Instant::now();
                self.pending =
                    std::io::Cursor::new(crate::encoded_streams::encode_wire_frame(&frame));
                continue;
            }
            if self.idle_since.elapsed() > STREAM_IDLE_TIMEOUT {
                debug!(track_id = %self.track_id, "encoded stream idle; closing");
                return Ok(0);
            }
            std::thread::sleep(STREAM_IDLE_POLL);
        }
    }
}

impl Drop for FrameStreamReader {
    fn drop(&mut self) {
        let dropped = self.streams.unsubscribe(&self.track_id);
        info!(
            track_id = %self.track_id,
            frames_dropped = dropped,
            "encoded stream ended"
        );
    }
}

/// Answer a stream request on its own thread, so the accept loop is not held open by it.
fn spawn_stream_responder(
    request: tiny_http::Request,
    track_id: String,
    streams: crate::encoded_streams::EncodedStreams,
) {
    let thread = std::thread::Builder::new()
        .name("frame-stream".into())
        .spawn(move || {
            info!(track_id = %track_id, "encoded stream opened");
            streams.subscribe(&track_id);
            let reader = FrameStreamReader {
                streams,
                track_id,
                pending: std::io::Cursor::new(Vec::new()),
                idle_since: std::time::Instant::now(),
            };
            // No content length, so `tiny_http` sends it chunked -- which is what lets the
            // page start reading frames before the track has finished producing them.
            let mut response = Response::new(200.into(), Vec::new(), reader, None, None);
            add_header(&mut response, "Content-Type", "application/octet-stream");
            add_header(&mut response, "Cache-Control", "no-store");
            if let Err(e) = request.respond(response) {
                debug!(error = %e, "an encoded stream ended with its reader gone");
            }
        });
    if let Err(e) = thread {
        error!(error = %e, "could not start a thread to serve an encoded stream");
    }
}

/// The prefix video frames are served under.
///
/// Namespaced so it can never collide with an embedded asset path: a route that shadowed a
/// real file would break the app in a way that looks nothing like a routing mistake.
const FRAME_PREFIX: &str = "/__elementium/frame/";

/// The track id a request is asking for a frame of, if it is asking for one.
fn frame_track_id(path: &str) -> Option<String> {
    let raw = path.strip_prefix(FRAME_PREFIX)?;
    if raw.is_empty() {
        return None;
    }
    Some(percent_decode(raw))
}

/// Decode the `%XX` escapes a track id may carry, leaving everything else alone.
///
/// Track ids are generated here and are hex and dashes today, but they reach this as part of
/// a URL the page built, and a decoder that is absent is a decoder that is wrong the first
/// time an id contains anything else.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while let Some(&byte) = bytes.get(i) {
        let decoded = if byte == b'%' {
            let hex = raw.get(i.saturating_add(1)..i.saturating_add(3));
            hex.and_then(|h| u8::from_str_radix(h, 16).ok())
        } else {
            None
        };
        if let Some(value) = decoded {
            out.push(value);
            i = i.saturating_add(3);
        } else {
            out.push(byte);
            i = i.saturating_add(1);
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| raw.to_owned())
}

/// The latest frame for `track_id`, as an 8-byte header (width, height, both `u32` LE)
/// followed by RGBA.
///
/// An absent frame is the zero header rather than a 404: "this track has produced nothing
/// yet" is an ordinary state on every connection for the first few hundred milliseconds, and
/// answering it with an error teaches the caller's error path to fire routinely.
fn frame_response<R: Runtime>(
    app: &AppHandle<R>,
    track_id: &str,
) -> Response<Cursor<Vec<u8>>> {
    let frame = app
        .try_state::<crate::protocols::VideoFrameState>()
        .and_then(|state| state.0.lock().ok().and_then(|f| f.get(track_id).cloned()));

    let body = frame.map_or_else(
        || vec![0u8; 8],
        |f| {
            let mut body = Vec::with_capacity(f.data.len().saturating_add(8));
            body.extend_from_slice(&f.width.to_le_bytes());
            body.extend_from_slice(&f.height.to_le_bytes());
            body.extend_from_slice(&f.data);
            body
        },
    );

    let mut response = Response::new(200.into(), Vec::new(), Cursor::new(body), None, None);
    add_header(&mut response, "Content-Type", "application/octet-stream");
    // Every request must reach the current frame. A cached one is a frozen picture.
    add_header(&mut response, "Cache-Control", "no-store");
    response
}

/// The asset path a request URL refers to, with the query string and fragment removed.
///
/// A bare `/` means the app's entry point; Tauri's resolver keys its assets by a path with
/// a leading slash, which is what a request URL already has.
fn asset_path(url: &str) -> String {
    let path = url
        .split_once(['?', '#'])
        .map_or(url, |(before, _)| before);
    if path.is_empty() || path == "/" {
        "/index.html".to_owned()
    } else {
        path.to_owned()
    }
}

/// Attach a header, ignoring one that cannot be represented rather than dropping the whole
/// response — a served asset with a missing header beats a request that never completes.
fn add_header<D: std::io::Read>(response: &mut Response<D>, name: &str, value: &str) {
    if let Ok(header) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
        response.add_header(header);
    } else {
        error!(name, "could not build a response header");
    }
}

#[cfg(test)]
mod tests {
    use super::asset_path;

    use super::{FRAME_PREFIX, frame_track_id, percent_decode};

    /// The frame route must never be mistaken for an embedded asset, and an asset path must
    /// never be mistaken for a frame request -- either way round the app breaks in a way
    /// that looks nothing like a routing mistake.
    #[test]
    fn a_frame_request_is_recognised_and_nothing_else_is() {
        assert_eq!(
            frame_track_id(&format!("{FRAME_PREFIX}pc-abc123-Ccu")),
            Some("pc-abc123-Ccu".to_owned())
        );
        assert_eq!(frame_track_id("/index.html"), None);
        assert_eq!(frame_track_id("/bundles/abc/bundle.js"), None);
        // The prefix with nothing after it names no track.
        assert_eq!(frame_track_id(FRAME_PREFIX), None);
    }

    #[test]
    fn a_percent_escaped_track_id_is_decoded() {
        assert_eq!(
            frame_track_id(&format!("{FRAME_PREFIX}pc%2Dabc%20x")),
            Some("pc-abc x".to_owned())
        );
    }

    /// A malformed escape is data, not a crash: the bytes are passed through unchanged.
    #[test]
    fn a_malformed_escape_is_left_alone() {
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%4"), "%4");
        assert_eq!(percent_decode(""), "");
    }

    #[test]
    fn the_root_is_the_entry_point() {
        assert_eq!(asset_path("/"), "/index.html");
        assert_eq!(asset_path(""), "/index.html");
    }

    #[test]
    fn a_query_string_is_not_part_of_the_path() {
        // Element Web appends cache-busting query strings to its own asset URLs, and a
        // resolver lookup including one finds nothing.
        assert_eq!(asset_path("/config.json?cachebuster=123"), "/config.json");
        assert_eq!(asset_path("/index.html#/room/!a:b"), "/index.html");
    }

    #[test]
    fn an_ordinary_path_is_left_alone() {
        assert_eq!(
            asset_path("/bundles/abc/bundle.js"),
            "/bundles/abc/bundle.js"
        );
    }
}
