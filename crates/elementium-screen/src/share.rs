//! One user's act of sharing a screen, window or application.
//!
//! Owns everything that has to be torn down when the share ends: the portal session, and
//! the identity of the `PipeWire` node the compositor granted. Nothing here decodes or
//! encodes -- the node id is handed to the media pipeline, which opens it exactly as it
//! opens a camera, because a portal-granted node delivers frames the same way.
//!
//! Two things this module exists to fix, both real:
//!
//! 1. **The portal session was never closed.** The previous code asked the portal for a
//!    node id and dropped every handle to the session on the way out of the function. The
//!    node kept working, which is why it went unnoticed, but nothing ever told the portal
//!    the share had ended.
//! 2. **The portal call ran on a nested runtime.** It built a `current_thread` runtime and
//!    `block_on`ed inside it. Called from an async Tauri command -- which is where it would
//!    be called from -- that panics outright. It never fired only because the Wayland path
//!    was unreachable: source enumeration fell back to the portal while starting a capture
//!    always constructed the X11 capturer. Everything here is `async` and awaited on the
//!    caller's runtime instead.

use elementium_types::CaptureSourceKind;

/// Why a share could not be started.
///
/// `Cancelled` is separated from the rest because it is not a fault: it is a person
/// deciding not to share, which is an ordinary outcome that must not be logged, reported or
/// counted as a failure.
#[derive(Debug, thiserror::Error)]
pub enum ShareError {
    /// The user dismissed the system picker without choosing a source.
    #[error("{PICKER_CANCELLED}: the screen-share picker was dismissed without a selection")]
    Cancelled,
    /// No way to capture the screen exists on this system.
    ///
    /// Names what is missing rather than failing generically, because the difference
    /// between "no portal is running" and "the portal refused" is the difference between a
    /// misconfigured desktop and a bug.
    #[error("no screen capture backend available: {0}")]
    NoBackend(String),
    /// The portal was reachable but the exchange failed.
    #[error("screen capture portal error: {0}")]
    Portal(String),
    /// The granted source could not be opened.
    #[error("could not open the granted capture source: {0}")]
    Capture(String),
}

/// Marker the frontend matches to tell a declined share from a broken one.
///
/// An agreed sentinel rather than prose, so the distinction survives somebody rewording the
/// message. Kept in step with `PICKER_CANCELLED` in `frontend/src/shim/media-devices.ts`.
pub const PICKER_CANCELLED: &str = "picker_cancelled";

/// Which mechanism a share is using to capture.
///
/// Chosen once when the session starts and then held, because the alternative -- deciding
/// per call -- is what let source enumeration answer from the portal while starting a
/// capture went to X11, so the two halves of one share disagreed about what was being
/// captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareBackend {
    /// The XDG desktop portal, which grants a `PipeWire` node.
    WaylandPortal,
    /// X11 via xcap, which polls a monitor or window directly.
    X11,
}

/// Where a running share's frames come from.
///
/// The two backends hand off to the media layer in opposite directions -- see `x11.rs`'s
/// module doc -- and this is what lets `ShareSession` carry either without pretending one is
/// the other: the portal grants a `PipeWire` node the media layer *pulls* from, while X11 has
/// no node at all, only the source id the caller chose from `get_capture_sources`, which
/// `elementium-media`'s push adapter (`video_source::VideoSource::start_push`) is driven by
/// instead.
#[derive(Debug, Clone)]
pub enum ShareSource {
    /// The `PipeWire` node the compositor granted, for the portal backend.
    Wayland { node_id: u32 },
    /// The source id chosen from `get_capture_sources`, for the X11 backend.
    X11 { source_id: String },
}

/// A running share.
///
/// Dropping this ends the share. The portal session is closed explicitly by
/// [`ShareSession::close`] where there is a runtime to await on; [`Drop`] is the backstop
/// for the paths that cannot.
pub struct ShareSession {
    backend: ShareBackend,
    /// Which node or source to capture, and how -- see [`ShareSource`].
    source: ShareSource,
    /// Whether the user picked a whole monitor or a single window.
    ///
    /// Reported by the portal rather than requested by us. It decides how share audio is
    /// scoped, and it is worth logging on its own: "the user shared a window" and "the user
    /// shared their desktop" are different events with different privacy weight.
    source_kind: Option<CaptureSourceKind>,
    /// The portal session, kept so it can be closed. `None` once closed, or for backends
    /// that have no session.
    #[cfg(target_os = "linux")]
    portal: Option<PortalSession>,
}

/// The portal handles that have to outlive the call that created them.
#[cfg(target_os = "linux")]
struct PortalSession {
    proxy: ashpd::desktop::screencast::Screencast,
    session: ashpd::desktop::Session<ashpd::desktop::screencast::Screencast>,
}

impl ShareSession {
    #[must_use]
    pub const fn backend(&self) -> ShareBackend {
        self.backend
    }

    /// Where this share's frames come from -- a `PipeWire` node to pull, or an X11 source id
    /// to push from. See [`ShareSource`].
    #[must_use]
    pub const fn source(&self) -> &ShareSource {
        &self.source
    }

    #[must_use]
    pub const fn source_kind(&self) -> Option<CaptureSourceKind> {
        self.source_kind
    }

    /// End the share, closing the portal session.
    ///
    /// Takes `self` so a closed session cannot be used afterwards. A no-op for a backend with
    /// no portal (X11): there is nothing there to close.
    pub async fn close(mut self) {
        #[cfg(target_os = "linux")]
        if let Some(portal) = self.portal.take() {
            match portal.session.close().await {
                Ok(()) => tracing::info!(source = ?self.source, "screencast portal session closed"),
                Err(e) => tracing::warn!(
                    source = ?self.source,
                    reason = %e,
                    "screencast portal session could not be closed; it will be dropped instead"
                ),
            }
            drop(portal.proxy);
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for ShareSession {
    fn drop(&mut self) {
        let Some(portal) = self.portal.take() else {
            return;
        };
        // Reached when a share is torn down without an await point -- an error path, or a
        // panic. Best effort: hand the close to the runtime if there is one, and say so if
        // there is not, rather than silently leaving a session open on the portal.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = portal.session.close().await;
            });
        } else {
            tracing::warn!(
                source = ?self.source,
                "share dropped outside a runtime; the portal session is left for the \
                 compositor to reap"
            );
        }
    }
}

/// Ask the user to choose something to share, and open a session for it.
///
/// Blocks for as long as the person takes to decide. That wait is deliberately unbounded:
/// a timeout here would cancel a dialog somebody was still reading.
///
/// # Errors
///
/// [`ShareError::Cancelled`] if the picker was dismissed, which is not a fault. Otherwise
/// the specific stage that failed -- see [`ShareError`].
#[cfg(target_os = "linux")]
pub async fn start_share() -> Result<ShareSession, ShareError> {
    use ashpd::desktop::screencast::{
        CursorMode, Screencast, SelectSourcesOptions, SourceType, StartCastOptions,
    };
    use ashpd::desktop::CreateSessionOptions;

    tracing::info!("requesting a screencast session from the XDG desktop portal");

    // Each portal step is timed and logged separately because the three of them are
    // indistinguishable from outside, and the difference matters: exactly one of them is
    // supposed to put a picker in front of the user. A `SelectSources` that returns in a
    // millisecond and a `Start` that returns in a millisecond together mean nobody was ever
    // asked what to share -- which is the report we could not investigate from a log that
    // said only "requesting" and then "granted". A step that takes seconds is a person
    // reading a dialog. No source name, window title or pixel is recorded here; the timings
    // and the step names are the whole of it.
    let mut step = std::time::Instant::now();

    let proxy = Screencast::new()
        .await
        .map_err(|e| ShareError::NoBackend(e.to_string()))?;
    let session = proxy
        .create_session(CreateSessionOptions::default())
        .await
        .map_err(|e| ShareError::Portal(e.to_string()))?;
    tracing::info!(
        elapsed_ms = %step.elapsed().as_millis(),
        "portal screencast session created"
    );
    step = std::time::Instant::now();

    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                // Both, so the compositor's picker can offer window selection where it has it.
                // Which of the two the user actually chose comes back on the stream.
                .set_sources(SourceType::Monitor | SourceType::Window)
                // Single source: the rest of the pipeline publishes one video track.
                .set_multiple(false)
                // Not persisted. A restore token would let a later share reuse this grant
                // without asking, and silently re-sharing a screen on the strength of a
                // decision made in an earlier session is not a default worth having.
                .set_persist_mode(ashpd::desktop::PersistMode::DoNot),
        )
        .await
        .map_err(|e| ShareError::Portal(e.to_string()))?;
    tracing::info!(
        elapsed_ms = %step.elapsed().as_millis(),
        "portal accepted the source selection request"
    );
    step = std::time::Instant::now();

    let response = proxy
        .start(&session, None, StartCastOptions::default())
        .await
        .map_err(|e| ShareError::Portal(e.to_string()))?
        .response()
        .map_err(|_| ShareError::Cancelled)?;
    tracing::info!(
        elapsed_ms = %step.elapsed().as_millis(),
        "portal returned from the picker"
    );

    let stream = response.streams().first().ok_or(ShareError::Cancelled)?;

    let source_kind = stream.source_type().and_then(|t| match t {
        SourceType::Monitor => Some(CaptureSourceKind::Monitor),
        SourceType::Window => Some(CaptureSourceKind::Window),
        SourceType::Virtual => None,
    });
    let node_id = stream.pipe_wire_node_id();

    tracing::info!(
        node_id,
        source_kind = ?source_kind,
        "portal granted a screencast stream"
    );

    Ok(ShareSession {
        backend: ShareBackend::WaylandPortal,
        source: ShareSource::Wayland { node_id },
        source_kind,
        portal: Some(PortalSession { proxy, session }),
    })
}

/// No screen capture backend is implemented for this platform yet.
///
/// # Errors
///
/// Always [`ShareError::NoBackend`]. macOS and Windows capture are out of scope for this
/// feature, and returning a named error is what stops the frontend showing the black
/// rectangle this whole feature exists to remove.
#[cfg(not(target_os = "linux"))]
pub async fn start_share() -> Result<ShareSession, ShareError> {
    Err(ShareError::NoBackend(
        "screen sharing is implemented for Linux only".to_owned(),
    ))
}

/// Recover the kind a `get_capture_sources`-issued X11 id encodes, from its prefix.
///
/// Best-effort, not a validation: an id that does not parse is left for
/// [`crate::x11::X11Capturer::start`] to reject with one of its four specific messages when
/// the share is actually opened, rather than being checked twice against two copies of the
/// same rule.
fn x11_source_kind(source_id: &str) -> Option<CaptureSourceKind> {
    if source_id.starts_with("monitor-") {
        Some(CaptureSourceKind::Monitor)
    } else if source_id.starts_with("window-") {
        Some(CaptureSourceKind::Window)
    } else {
        None
    }
}

/// Start a share of one specific X11 source, chosen from
/// [`crate::x11::X11Capturer::sources`] (what `get_capture_sources` calls).
///
/// Unlike [`start_share`], there is no picker behind this: X11 has no compositor dialog to
/// hand the choice to, so `source_id` *is* the whole selection. Its absence is refused here,
/// synchronously and by name, rather than being allowed through to become a session that
/// starts and never captures anything -- the exact silent failure T042 removed from
/// `X11Capturer::start` itself.
///
/// Not `async`: unlike the portal path there is no dialog to await and no D-Bus round trip.
///
/// # Errors
///
/// [`ShareError::Capture`] if no source id was given.
#[cfg(target_os = "linux")]
pub fn start_x11_share(source_id: Option<&str>) -> Result<ShareSession, ShareError> {
    let source_id = source_id.ok_or_else(|| {
        ShareError::Capture(
            "an X11 share needs a source id from get_capture_sources; there is no picker to \
             fall back on"
                .to_owned(),
        )
    })?;

    let source_kind = x11_source_kind(source_id);
    tracing::info!(source_id, source_kind = ?source_kind, "starting an X11 screen share");

    Ok(ShareSession {
        backend: ShareBackend::X11,
        source: ShareSource::X11 {
            source_id: source_id.to_owned(),
        },
        source_kind,
        portal: None,
    })
}

/// No X11 capture backend exists off Linux, matching [`start_share`]'s non-Linux stub.
///
/// # Errors
///
/// Always [`ShareError::NoBackend`].
#[cfg(not(target_os = "linux"))]
pub fn start_x11_share(_source_id: Option<&str>) -> Result<ShareSession, ShareError> {
    Err(ShareError::NoBackend(
        "screen sharing is implemented for Linux only".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::{PICKER_CANCELLED, ShareError, start_x11_share};

    /// The whole point of [`start_x11_share`]'s `Option` parameter: X11 has no picker to
    /// fall back on, so a missing source id has to be refused here rather than producing a
    /// session that silently never captures anything.
    #[test]
    fn an_x11_share_with_no_source_id_is_refused() {
        let result = start_x11_share(None);
        assert!(
            result.is_err(),
            "an X11 share started without a source id must not succeed"
        );
    }

    /// The seam this task exists to close: an X11-chosen source id has to survive into the
    /// session exactly as given, since it is what the media layer will hand to
    /// `X11Capturer::start` to open the real capture. Linux-only because
    /// `start_x11_share`'s real implementation -- as opposed to its `NoBackend` stub -- only
    /// exists there.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_x11_share_carries_its_source_id_and_kind_through() {
        use elementium_types::CaptureSourceKind;

        use super::{ShareBackend, ShareSource};

        let result = start_x11_share(Some("monitor-3"));
        assert!(result.is_ok(), "a valid id must be accepted");
        if let Ok(session) = result {
            assert_eq!(session.backend(), ShareBackend::X11);
            assert_eq!(session.source_kind(), Some(CaptureSourceKind::Monitor));
            let mut carried_x11_source = false;
            if let ShareSource::X11 { source_id } = session.source() {
                carried_x11_source = true;
                assert_eq!(source_id, "monitor-3");
            }
            assert!(carried_x11_source, "an X11 share must carry an X11 source");
        }
    }

    /// The frontend decides whether to report a failure by matching this prefix, so the
    /// message has to start with it. A reworded error that drops the marker turns every
    /// cancelled picker into a logged failure, with nothing to say the reporting changed.
    #[test]
    fn a_cancelled_picker_is_recognisable_by_its_agreed_prefix() {
        assert!(
            ShareError::Cancelled.to_string().starts_with(PICKER_CANCELLED),
            "cancellation must be identifiable without parsing prose"
        );
    }

    /// Cancellation must not be confusable with a real failure, in either direction.
    #[test]
    fn a_genuine_failure_does_not_look_like_a_cancellation() {
        for err in [
            ShareError::NoBackend("no portal".to_owned()),
            ShareError::Portal("dbus died".to_owned()),
            ShareError::Capture("node gone".to_owned()),
        ] {
            assert!(
                !err.to_string().starts_with(PICKER_CANCELLED),
                "{err} must not be mistaken for the user declining"
            );
        }
    }
}
