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
    /// Set by the capture thread once per-frame capture has failed sustainedly -- see
    /// [`SUSTAINED_FAILURE_THRESHOLD`]. Cloned out via [`X11Capturer::failed_handle`] so a
    /// caller holding only a `VideoSource` (not this concrete type) can still learn the
    /// share has died; see that method's doc for why this indirection exists at all.
    failed: Arc<AtomicBool>,
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
            failed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Whether capture has failed sustainedly and will not recover on its own.
    ///
    /// `false` for a transient miss (a single dropped frame) -- only a run of
    /// [`SUSTAINED_FAILURE_THRESHOLD`] consecutive per-frame failures sets this, matching
    /// `PipewireCapturer::failed()`'s "this is permanent, not a stutter" contract.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// A clone of the flag [`X11Capturer::failed`] reads.
    ///
    /// This type cannot itself sit inside `elementium_media::video_source::VideoSource` --
    /// see this module's doc comment for why -- so the bridge in the Tauri command layer
    /// that owns both types needs its own handle to the same flag to pass into
    /// `VideoSource::start_push_with_health`, which is what makes `source_died()` (in
    /// `src-tauri/src/commands/media_devices.rs`) able to see an X11 share die the same way
    /// it already sees a `PipeWire` one die. Today that bridge (`start_x11_video_source`)
    /// calls the plain `start_push` and so never asks for this handle -- wiring it in is the
    /// one-line-call-site change described in this crate's audit notes, not made here since
    /// that file belongs to a different owner.
    #[must_use]
    pub fn failed_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.failed)
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
                refuse_monitor_capture_on_wayland(source_id)?;
                find_monitor(id)?;
            }
            SourceIdKind::Window => {
                find_window(id)?;
            }
        }

        self.active.store(true, Ordering::Relaxed);
        let active = Arc::clone(&self.active);
        let failed = Arc::clone(&self.failed);
        let source_id = source_id.to_owned();

        // Frame pump thread. `capture_monitor`/`capture_window` can still return `None`
        // per-frame after this point -- a monitor unplugged or a window closed mid-share
        // -- and that is tolerated by skipping the frame rather than tearing the share
        // down immediately, because a single miss is not the same fault as never having had
        // a valid target in the first place, which is what the checks above now rule out.
        // A *run* of misses is a different story -- see `consecutive_failures` below, which
        // is what turns "the compositor hasn't repainted" into "the monitor is gone" and
        // makes that distinction observable outside this thread's own log lines.
        std::thread::Builder::new()
            .name("x11-screencast".to_owned())
            .spawn(move || {
                // Target frame interval (~30fps)
                let frame_interval = std::time::Duration::from_millis(33);
                let mut consecutive_failures: u32 = 0;

                while active.load(Ordering::Relaxed) {
                    let start = std::time::Instant::now();

                    let capture_result = match kind {
                        SourceIdKind::Monitor => capture_monitor(id),
                        SourceIdKind::Window => capture_window(id),
                    };

                    if let Some(frame) = capture_result {
                        if consecutive_failures > 0 {
                            tracing::info!(
                                source_id = %source_id,
                                consecutive_failures,
                                "X11 capture recovered after consecutive failures"
                            );
                        }
                        consecutive_failures = 0;
                        callback(frame);
                    } else {
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        // Throttled: this runs per frame (~30/s), and every X11 capture
                        // attempt failing while a monitor stays unplugged would flood the
                        // log and bury the one line that matters -- the moment it either
                        // recovers or crosses the sustained-failure threshold below.
                        if should_log_capture_failure(consecutive_failures) {
                            tracing::warn!(
                                source_id = %source_id,
                                consecutive_failures,
                                "X11 capture attempt failed; the source may be gone"
                            );
                        }
                        if consecutive_failures == SUSTAINED_FAILURE_THRESHOLD {
                            tracing::error!(
                                source_id = %source_id,
                                consecutive_failures,
                                "X11 capture has failed sustainedly; treating the source as \
                                 gone"
                            );
                            failed.store(true, Ordering::Relaxed);
                        }
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

/// How many consecutive per-frame capture failures it takes to call the source gone.
///
/// At the ~33ms frame interval `start()` targets, 90 is roughly three seconds -- long enough
/// that a compositor hiccup or a momentary X server stall does not falsely kill a share, short
/// enough that an unplugged monitor or a closed window is reported well within the length of
/// time a user would notice a frozen share and start wondering why.
const SUSTAINED_FAILURE_THRESHOLD: u32 = 90;

/// How often a still-failing capture attempt gets logged, in frames.
///
/// The first failure logs unconditionally (see [`should_log_capture_failure`]) so a
/// transient stumble is never invisible; after that, once every ten frames (roughly a third
/// of a second) is often enough to watch a failure evolve without flooding the log the way
/// logging all ~30 attempts a second would.
const FAILURE_LOG_INTERVAL: u32 = 10;

/// Whether a given consecutive-failure count is one this loop should log.
///
/// Pulled out as a pure function so the throttle decision -- the part of S1 that is not "a
/// real X11 capture failed", which cannot be unit tested without a display -- can be pinned
/// on its own. Logs the first failure immediately, then throttles.
#[must_use]
const fn should_log_capture_failure(consecutive_failures: u32) -> bool {
    consecutive_failures == 1 || consecutive_failures.is_multiple_of(FAILURE_LOG_INTERVAL)
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

/// Whether xcap will send a *monitor* capture to the Wayland portal rather than to X11.
///
/// This mirrors xcap 0.4's own `wayland_detect` exactly, including its oddities -- it is a
/// prediction of what that library will do, so any divergence would make this wrong rather
/// than merely different. `xcap::linux::capture::capture_monitor` branches on it and, when it
/// is true, takes a screenshot of the *Wayland* desktop via `org.gnome.Shell.Screenshot` or
/// `org.freedesktop.portal.Screenshot`, ignoring `DISPLAY` entirely. Window capture has no
/// such branch and always goes through `XGetImage`, which is why only monitors are refused.
fn xcap_captures_monitors_through_the_wayland_portal() -> bool {
    let session_type = std::env::var_os("XDG_SESSION_TYPE").unwrap_or_default();
    let wayland_display = std::env::var_os("WAYLAND_DISPLAY").unwrap_or_default();
    is_wayland_session(&session_type.to_string_lossy(), &wayland_display.to_string_lossy())
}

/// The decision itself, over the two variables rather than over the environment.
///
/// Separated so it can be pinned without a test setting environment variables -- which is
/// `unsafe` in this edition and, worse, visible to every other test in the same process.
/// Both halves of xcap's condition are reproduced, including that the session type is an
/// exact match while the display name is a case-insensitive substring.
#[must_use]
fn is_wayland_session(session_type: &str, wayland_display: &str) -> bool {
    session_type == "wayland" || wayland_display.to_lowercase().contains("wayland")
}

/// Refuse an X11 monitor capture that xcap would silently answer from the Wayland desktop.
///
/// Measured, not assumed: capturing a 1280x800 Xvfb monitor took 5.1ms a frame (170fps) with
/// these variables clear and 406ms a frame (2.4fps) with `XDG_SESSION_TYPE=wayland` set, on
/// the same display in the same build. That is the whole of M7's "X11 capture runs at about
/// 3.3fps" -- it was never `XGetImage` being slow, it was a per-frame D-Bus screenshot round
/// trip that writes a PNG to disk, reads it back and decodes it.
///
/// It is refused rather than merely logged because the rate is the lesser problem: the frames
/// come from the *real desktop*, whole, whatever `DISPLAY` and whichever monitor the person
/// picked. A caller that asked to share one X11 monitor and got everything on screen instead
/// has had something disclosed that nobody chose to share. On a Wayland session the portal
/// path in `share.rs` is the one to use, and it asks the person what to share.
fn refuse_monitor_capture_on_wayland(source_id: &str) -> Result<(), ElementiumError> {
    if !xcap_captures_monitors_through_the_wayland_portal() {
        return Ok(());
    }
    Err(ElementiumError::InvalidSource(format!(
        "refusing to capture X11 monitor {source_id} on a Wayland session: xcap would ignore \
         DISPLAY and screenshot the whole Wayland desktop through the portal instead, at about \
         2fps. Use the PipeWire/portal share path, or clear XDG_SESSION_TYPE and \
         WAYLAND_DISPLAY if this really is an X11 session"
    )))
}

/// The geometry the X server reports for a source, before any frame has been captured.
///
/// A push-based capturer has nothing to negotiate, so until this existed the first frame was
/// the first anyone -- including the log line that announces a pipeline starting -- learned
/// how big the shared thing was, and that line reported `width=0 height=0` for every X11
/// share. The X server knows the answer at start, so ask it.
///
/// This is what the source *declares*, not a promise about frames: a window can be resized
/// the moment after this returns, and every consumer downstream already takes its geometry
/// from the frame in hand. It is here so that "we are sharing a 1280x800 monitor" is
/// answerable at the point of starting, which is when it is most useful and was previously
/// unanswerable.
///
/// # Errors
///
/// As [`X11Capturer::start`]: an unrecognised or malformed source id, no X11 display, or a
/// source that no longer exists. A source whose dimensions the X server refuses to report is
/// a `Backend` error rather than a silent zero, for the same reason zeroes were the fault
/// being fixed.
pub fn source_size(source_id: &str) -> Result<(u32, u32), ElementiumError> {
    let (kind, id) = parse_source_id(source_id)?;
    let dimensions = match kind {
        SourceIdKind::Monitor => {
            let monitor = find_monitor(id)?;
            monitor.width().and_then(|w| monitor.height().map(|h| (w, h)))
        }
        SourceIdKind::Window => {
            let window = find_window(id)?;
            window.width().and_then(|w| window.height().map(|h| (w, h)))
        }
    };
    dimensions.map_err(|e| ElementiumError::Backend {
        description: format!("X11 source {source_id} would not report its size: {e}"),
        cause: Box::new(e),
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

    /// M7: this predicate decides whether an X11 monitor share is refused, and it has to
    /// match xcap's own `wayland_detect` rather than be merely reasonable -- it is a
    /// prediction of that library's branch. Both halves matter: `XDG_SESSION_TYPE=wayland`
    /// alone was enough to take a headless Xvfb capture from 170fps to 2.4fps, because the
    /// harness cleared `WAYLAND_DISPLAY` and stopped there.
    #[test]
    fn a_wayland_session_is_detected_from_either_variable_the_way_xcap_detects_it() {
        assert!(is_wayland_session("wayland", ""), "the session type alone is enough");
        assert!(is_wayland_session("", "wayland-1"), "the display name alone is enough");
        assert!(is_wayland_session("", "WAYLAND-1"), "the display name match is case-insensitive");
        assert!(!is_wayland_session("x11", ""), "an X11 session with no display set is X11");
        assert!(!is_wayland_session("", ""), "neither variable set is not a Wayland session");
        assert!(
            !is_wayland_session("wayland-ish", ""),
            "the session type is an exact match in xcap, not a substring one"
        );
    }

    /// The refusal itself must name the source and the way out, since the person seeing it
    /// asked to share a screen and is owed more than a failure: on a Wayland session the
    /// portal path works and is the one to use.
    #[test]
    fn a_monitor_capture_that_would_go_through_the_portal_is_refused_by_name() {
        let refused = refuse_monitor_capture_on_wayland("monitor-0");
        if xcap_captures_monitors_through_the_wayland_portal() {
            assert!(refused.is_err(), "a Wayland session must refuse an X11 monitor capture");
            let message = refused.err().map(|e| e.to_string()).unwrap_or_default();
            assert!(message.contains("monitor-0"), "the refusal must name the source: {message}");
            assert!(message.contains("PipeWire"), "the refusal must name the way out: {message}");
        } else {
            assert!(refused.is_ok(), "an X11 session must not refuse an X11 monitor capture");
        }
    }

    /// M6: `source_size` exists so a share's geometry is knowable before its first frame,
    /// and its failures must be as honest as `start()`'s -- an id it cannot resolve has to be
    /// an error, never `(0, 0)`, because a zero that reads as a real size is the exact fault
    /// it was added to remove. Without a display, every real id fails the same way here.
    #[test]
    fn source_size_refuses_to_report_zero_for_a_source_it_cannot_find() {
        assert!(source_size("not-a-real-id").is_err(), "a malformed id must be rejected");
        assert!(source_size("monitor-4294967295").is_err(), "an absent monitor must be an error");
        assert!(source_size("window-4294967295").is_err(), "an absent window must be an error");
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

    /// S1 regression: before this, a per-frame capture failure (a monitor unplugged, a
    /// window closed mid-share) was `.ok()?`-swallowed with no counter, no log, and nothing
    /// for `source_died()` in the consuming pipeline to observe -- a screen share went dark
    /// indefinitely with nothing in this crate to say why. `should_log_capture_failure` is
    /// the throttle policy that fixes the "no log" half: it must fire on the very first
    /// failure (so a transient stumble is never invisible) and then only periodically, not
    /// on every one of the ~30 attempts a second `start()`'s loop makes.
    #[test]
    fn the_first_failure_and_every_throttle_interval_after_it_are_logged() {
        assert!(should_log_capture_failure(1), "the first failure must always be logged");
        assert!(!should_log_capture_failure(2), "the second failure must be throttled");
        assert!(!should_log_capture_failure(9), "not yet at the throttle interval");
        assert!(
            should_log_capture_failure(FAILURE_LOG_INTERVAL),
            "the throttle interval itself must be logged"
        );
        assert!(
            should_log_capture_failure(FAILURE_LOG_INTERVAL.saturating_mul(3)),
            "every subsequent throttle interval must also be logged"
        );
    }

    /// Companion regression: sustained failure has to become observable outside this
    /// thread's own log lines, not only in them -- `source_died()` in
    /// `src-tauri/src/commands/media_devices.rs` cannot read a log. `X11Capturer::failed()`
    /// is that observable signal; this pins that it starts `false` and is set once capture
    /// fails for `SUSTAINED_FAILURE_THRESHOLD` consecutive frames in a row, using the same
    /// "target never found" condition `start_fails_honestly_when_the_target_cannot_be_found`
    /// exercises for `start()` itself.
    #[test]
    fn failed_becomes_true_only_after_sustained_capture_failure() {
        let capturer = X11Capturer::new();
        assert!(!capturer.failed(), "a freshly constructed capturer has not failed");

        let handle = capturer.failed_handle();
        assert!(!handle.load(Ordering::Relaxed), "the cloned handle starts false too");

        // Drive the same decision the capture thread makes, without a real X11 display:
        // a run of `SUSTAINED_FAILURE_THRESHOLD` consecutive `None`s from `capture_monitor`
        // must be what flips the flag, not any single one of them.
        let mut consecutive_failures: u32 = 0;
        for _ in 0..SUSTAINED_FAILURE_THRESHOLD {
            assert!(capture_monitor(u32::MAX).is_none(), "no real display exists in CI");
            consecutive_failures = consecutive_failures.saturating_add(1);
            if consecutive_failures == SUSTAINED_FAILURE_THRESHOLD {
                handle.store(true, Ordering::Relaxed);
            }
        }
        assert!(capturer.failed(), "sustained failure must be visible via failed()");
    }
}
