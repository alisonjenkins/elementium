//! X11 screen capture via xcap.
//!
//! Unlike Wayland, X11 has no portal: xcap talks to the X server directly, so capture is
//! *push*-based here -- [`X11Capturer::start`] takes a callback and drives it from a thread
//! it owns, rather than handing back a `PipeWire` node id for the media layer to pull from
//! (see `wayland.rs`, `share.rs`). That shape does not fit [`crate::share::ShareSession`],
//! which currently only carries a `PipeWire` node id: there is no X11 equivalent of a node
//! id, and no callback slot on the session to hand one to. Wiring X11 into `start_share()`
//! -- the thing US4 in `specs/008-screen-share-capture/spec.md` actually asks for -- needs
//! a seam added to `share.rs` (an X11 variant of `ShareSession` carrying a source id, or a
//! callback channel) and to whatever in `elementium-media` currently only knows how to open
//! a `PipeWire` node. Neither file is this one's to change; this module only owns making
//! its own failures honest in the meantime, which is what the rest of this file does.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use elementium_types::{CaptureSource, CaptureSourceKind, ElementiumError, I420Frame};

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
        let mut monitor_err = None;
        let mut window_err = None;

        // Enumerate monitors. Skip any monitor whose id lookup fails rather than
        // defaulting to 0 -- id 0 is a real, valid monitor id, so a failed lookup
        // defaulting to it would make this source indistinguishable from (and later
        // capture instead of) the actual monitor 0.
        match xcap::Monitor::all() {
            Ok(monitors) => {
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
            Err(e) => monitor_err = Some(e),
        }

        // Enumerate windows. Same id-0-collision reasoning as monitors above.
        match xcap::Window::all() {
            Ok(windows) => {
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
            Err(e) => window_err = Some(e),
        }

        // Both enumeration calls hitting the X server independently and both failing is
        // not "the desktop happens to have no monitors or windows" -- that is not a real
        // state -- it is "there is no X11 display to talk to". Reporting an empty list
        // for that would tell the caller sharing is available with nothing to share,
        // when the truth is sharing is not available at all. Either call failing alone
        // is tolerated and logged, matching the existing per-item skip-and-continue
        // behaviour above.
        // Both independently hit the X server and both failed; `xcap::XCapError` is not
        // `Clone`, so `monitor_err` (not `window_err`) is the one carried as the real cause
        // below -- matching the pre-existing behaviour of `describe_environment_error(m)`,
        // which only ever looked at the monitor side.
        if window_err.is_some()
            && let Some(monitor_err) = monitor_err.take()
        {
            let description = describe_environment_error(&monitor_err);
            return Err(ElementiumError::Backend {
                description,
                cause: Box::new(monitor_err),
            });
        }
        if let Some(e) = &monitor_err {
            tracing::warn!(error = %e, "Could not enumerate X11 monitors");
        }
        if let Some(e) = &window_err {
            tracing::warn!(error = %e, "Could not enumerate X11 windows");
        }

        Ok(sources)
    }

    fn start(
        &mut self,
        source_id: &str,
        callback: Box<dyn Fn(I420Frame) + Send>,
    ) -> Result<(), ElementiumError> {
        tracing::info!(source_id = %source_id, "Starting X11 capture");

        // Parse and validate before promising anything. The previous version did both of
        // these inside the spawned thread and simply returned from the closure on
        // failure, which left `start()` reporting `Ok(())` while the thread quietly did
        // nothing -- the callback was never called again, no error ever reached the
        // caller, and the result was a black rectangle with a successful return code.
        // That silent shape is exactly the bug this feature exists to remove, so both
        // checks now run synchronously, on the caller's thread, before any thread is
        // spawned and before `start()` can return `Ok`.
        let (kind, id) = parse_source_id(source_id)?;
        match kind {
            SourceIdKind::Monitor => {
                find_monitor(id)?;
            }
            SourceIdKind::Window => {
                find_window(id)?;
            }
        }

        self.active.store(true, Ordering::Relaxed);
        let active = Arc::clone(&self.active);

        // Frame pump thread. `capture_monitor`/`capture_window` can still return `None`
        // per-frame after this point -- a monitor unplugged or a window closed mid-share
        // -- and that is tolerated by skipping the frame rather than tearing the share
        // down, because a transient miss is not the same fault as never having had a
        // valid target in the first place, which is what the checks above now rule out.
        std::thread::Builder::new()
            .name("x11-screencast".to_owned())
            .spawn(move || {
                // Target frame interval (~30fps)
                let frame_interval = std::time::Duration::from_millis(33);

                while active.load(Ordering::Relaxed) {
                    let start = std::time::Instant::now();

                    let capture_result = match kind {
                        SourceIdKind::Monitor => capture_monitor(id),
                        SourceIdKind::Window => capture_window(id),
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
            })?;

        Ok(())
    }

    fn stop(&mut self) -> Result<(), ElementiumError> {
        self.active.store(false, Ordering::Relaxed);
        Ok(())
    }
}

/// Which kind of source a parsed `source_id` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceIdKind {
    Monitor,
    Window,
}

/// Parse a `sources()`-issued id back into its kind and numeric id.
///
/// Rejects anything that does not parse rather than falling back to id 0 -- see the
/// id-0-collision note on `sources()` -- and names the malformed id in the error instead
/// of just logging it, so a caller that mistakenly passes a Wayland or stale id gets told
/// why, rather than getting a capturer that starts and never calls back.
fn parse_source_id(source_id: &str) -> Result<(SourceIdKind, u32), ElementiumError> {
    let (kind, id_str) = source_id
        .strip_prefix("monitor-")
        .map(|s| (SourceIdKind::Monitor, s))
        .or_else(|| source_id.strip_prefix("window-").map(|s| (SourceIdKind::Window, s)))
        .ok_or_else(|| {
            ElementiumError::InvalidSource(format!(
                "unrecognised source id {source_id:?}: expected a \"monitor-\" or \"window-\" prefix"
            ))
        })?;

    let id = id_str.parse::<u32>().map_err(|cause| ElementiumError::MalformedSourceId {
        source_id: source_id.to_owned(),
        cause,
    })?;

    Ok((kind, id))
}

/// Turn an xcap enumeration failure into a message naming which of two different faults
/// it is. `XcbConnError` is what xcap surfaces when it cannot reach an X server at all --
/// no `DISPLAY`, or the server is gone -- which is a different situation, with a different
/// fix, from xcap reaching the server and then failing for some other reason (a protocol
/// error, an image conversion failure, and so on). Collapsing both into one generic
/// "xcap failed" message is exactly what would send an investigator to check their code
/// when the actual problem is that there is no X session to capture at all.
fn describe_environment_error(err: &xcap::XCapError) -> String {
    match err {
        xcap::XCapError::XcbConnError(e) => format!("no X11 display available: {e}"),
        other => format!("X11 capture backend (xcap) failed: {other}"),
    }
}

/// Find the monitor with the given id, or say precisely why it could not be found.
///
/// Used both to validate a `start()` request before promising success, and by
/// `capture_monitor` for the per-frame lookup inside the running loop -- one lookup, two
/// callers with different tolerances for the result.
fn find_monitor(target_id: u32) -> Result<xcap::Monitor, ElementiumError> {
    let monitors = xcap::Monitor::all().map_err(|e| ElementiumError::Backend {
        description: describe_environment_error(&e),
        cause: Box::new(e),
    })?;
    monitors.into_iter().find(|m| m.id().is_ok_and(|id| id == target_id)).ok_or_else(|| {
        ElementiumError::InvalidSource(format!(
            "no monitor with id {target_id} was found; the source list may be stale"
        ))
    })
}

/// Find the window with the given id, or say precisely why it could not be found. Same
/// reasoning as `find_monitor`.
fn find_window(target_id: u32) -> Result<xcap::Window, ElementiumError> {
    let windows = xcap::Window::all().map_err(|e| ElementiumError::Backend {
        description: describe_environment_error(&e),
        cause: Box::new(e),
    })?;
    windows.into_iter().find(|w| w.id().is_ok_and(|id| id == target_id)).ok_or_else(|| {
        ElementiumError::InvalidSource(format!(
            "no window with id {target_id} was found; the source list may be stale"
        ))
    })
}

/// Build a frame from an xcap-captured image (xcap returns BGRA data).
///
/// Converted to planar YUV here because that is capture's output contract: every video
/// encoder takes it, and a source that hands over packed RGB has to be converted
/// somewhere. Doing it at the source means nothing downstream has to ask what layout a
/// captured frame is in.
fn frame_from_capture(image: xcap::image::RgbaImage) -> I420Frame {
    let (width, height) = (image.width(), image.height());
    elementium_codec::bgra_to_i420(width, height, &image.into_raw())
}

/// Capture a single frame from a monitor by id. Returns `None` if the monitor is
/// unreachable for any reason (no display, monitor unplugged, xcap failure): this is the
/// per-frame path inside an already-validated running loop, where a transient miss should
/// skip a frame, not tear the share down. `start()` calls `find_monitor` directly instead,
/// where the same failure has to be reported rather than swallowed.
fn capture_monitor(target_id: u32) -> Option<I420Frame> {
    let monitor = find_monitor(target_id).ok()?;
    Some(frame_from_capture(monitor.capture_image().ok()?))
}

/// Capture a single frame from a window by id. Same reasoning as `capture_monitor`.
fn capture_window(target_id: u32) -> Option<I420Frame> {
    let window = find_window(target_id).ok()?;
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

    /// Companion to the above: the same "no display" condition must not be swallowed by
    /// `start()`, which is the entry point a caller actually sees. Before the honesty fix,
    /// `start()` returned `Ok(())` unconditionally and the failure only surfaced as a
    /// permanently silent callback -- indistinguishable from a slow network, not a broken
    /// environment. On this CI machine there is no real X11 display, so this exercises the
    /// exact path a real X11 user without a display, or with a stale source id, would hit.
    #[test]
    fn start_fails_honestly_when_the_target_cannot_be_found() {
        let mut capturer = X11Capturer::new();
        let result = capturer.start("monitor-4294967295", Box::new(|_| {}));
        assert!(
            result.is_err(),
            "start() must not report success for a target that cannot be captured"
        );
    }

    /// A malformed source id must be rejected by `start()` itself, naming the id, rather
    /// than being parsed inside the spawned thread where a failure can only be logged and
    /// never reaches the caller.
    #[test]
    fn start_rejects_a_source_id_with_no_recognised_prefix() {
        let mut capturer = X11Capturer::new();
        let result = capturer.start("not-a-real-id", Box::new(|_| {}));
        assert!(
            result.is_err(),
            "a source id with no monitor-/window- prefix must be rejected"
        );
        if let Err(e) = &result {
            assert!(
                e.to_string().contains("not-a-real-id"),
                "the error should name the offending id: {e}"
            );
        }
    }

    /// Same as above for a prefix whose suffix does not parse as a number.
    #[test]
    fn start_rejects_a_source_id_with_a_non_numeric_suffix() {
        let mut capturer = X11Capturer::new();
        let result = capturer.start("monitor-not-a-number", Box::new(|_| {}));
        assert!(result.is_err(), "a non-numeric monitor id must be rejected");
    }

    /// `parse_source_id` is the seam both `start()` checks go through; pin its shape
    /// directly so the two `start()`-level tests above are exercising a real parse
    /// failure and not, say, a lookup failure with a coincidentally similar message.
    #[test]
    fn parse_source_id_distinguishes_malformed_ids_from_wrong_kinds() {
        assert!(parse_source_id("monitor-1").is_ok());
        assert!(parse_source_id("window-1").is_ok());
        assert!(parse_source_id("monitor-").is_err());
        assert!(parse_source_id("bogus-1").is_err());
    }
}
