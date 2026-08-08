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

/// A running share.
///
/// Dropping this ends the share. The portal session is closed explicitly by
/// [`ShareSession::close`] where there is a runtime to await on; [`Drop`] is the backstop
/// for the paths that cannot.
pub struct ShareSession {
    backend: ShareBackend,
    /// The `PipeWire` node the compositor granted, for the portal backend.
    node_id: u32,
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
    proxy: ashpd::desktop::screencast::Screencast<'static>,
    session: ashpd::desktop::Session<'static, ashpd::desktop::screencast::Screencast<'static>>,
}

impl ShareSession {
    #[must_use]
    pub const fn backend(&self) -> ShareBackend {
        self.backend
    }

    /// The `PipeWire` node id to open for video.
    #[must_use]
    pub const fn node_id(&self) -> u32 {
        self.node_id
    }

    #[must_use]
    pub const fn source_kind(&self) -> Option<CaptureSourceKind> {
        self.source_kind
    }

    /// End the share, closing the portal session.
    ///
    /// Takes `self` so a closed session cannot be used afterwards.
    pub async fn close(mut self) {
        #[cfg(target_os = "linux")]
        if let Some(portal) = self.portal.take() {
            match portal.session.close().await {
                Ok(()) => tracing::info!(node_id = self.node_id, "screencast portal session closed"),
                Err(e) => tracing::warn!(
                    node_id = self.node_id,
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
                node_id = self.node_id,
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
    use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};

    tracing::info!("requesting a screencast session from the XDG desktop portal");

    let proxy = Screencast::new()
        .await
        .map_err(|e| ShareError::NoBackend(e.to_string()))?;
    let session = proxy
        .create_session()
        .await
        .map_err(|e| ShareError::Portal(e.to_string()))?;

    proxy
        .select_sources(
            &session,
            CursorMode::Embedded,
            // Both, so the compositor's picker can offer window selection where it has it.
            // Which of the two the user actually chose comes back on the stream.
            SourceType::Monitor | SourceType::Window,
            // Single source: the rest of the pipeline publishes one video track.
            false,
            None,
            // Not persisted. A restore token would let a later share reuse this grant
            // without asking, and silently re-sharing a screen on the strength of a
            // decision made in an earlier session is not a default worth having.
            ashpd::desktop::PersistMode::DoNot,
        )
        .await
        .map_err(|e| ShareError::Portal(e.to_string()))?;

    let response = proxy
        .start(&session, None)
        .await
        .map_err(|e| ShareError::Portal(e.to_string()))?
        .response()
        .map_err(|_| ShareError::Cancelled)?;

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
        node_id,
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

#[cfg(test)]
mod tests {
    use super::{PICKER_CANCELLED, ShareError};

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
