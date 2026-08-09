use thiserror::Error;

/// The screen-capture failure surface shared by `elementium-screen`'s `ScreenCapturer`
/// backends (Wayland/portal, X11/`xcap`).
///
/// `WebRtc`, `Codec`, `MediaDevice`, and `Signaling` were deleted: none of the four was ever
/// constructed anywhere in the workspace (confirmed by grep across every crate, including
/// `elementium-webrtc` and `src-tauri`, which depend on this crate but never reference this
/// type at all -- only `elementium-screen` uses it). `ScreenCapture(String)` -- the one
/// variant actually in use -- has been split by real cause below.
///
/// This type deliberately carries real, walkable sources (`#[source]`) rather than
/// stringified messages, but cannot name `ashpd::Error`, `PipewireError`, or `xcap::XCapError`
/// directly: this crate is documented ("no heavy deps") to stay free of such dependencies, and
/// those three alone would pull in D-Bus, `PipeWire`, and X11 bindings. `Backend` boxes the
/// cause behind `dyn std::error::Error` instead -- type erasure, not stringification: the
/// real error object survives, `source()` still walks to it and, if it has its own source
/// chain, through it, and its `Display` output is what appears in `description`. This is not
/// the `Box<dyn Error>`-at-a-fallible-boundary the project constitution warns against (that
/// warning is about erasing a cause with no plan to preserve it); here it is the one place in
/// the workspace to erase a cause on purpose, at a deliberate, documented dependency boundary.
#[derive(Error, Debug)]
pub enum ElementiumError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The capture backend (the XDG desktop portal, `PipeWire`, or `xcap`) failed.
    /// `description` is a categorised, human-readable summary (e.g. distinguishing "no X11
    /// display at all" from "the display answered but capture failed"); `cause` is the real
    /// error object, preserved for `source()` to walk.
    #[error("{description}")]
    Backend {
        description: String,
        #[source]
        cause: Box<dyn std::error::Error + Send + Sync>,
    },

    /// A source id/selection names nothing real: an unrecognised prefix, a numeric id no
    /// current monitor/window has, or a portal exchange that ended with no stream selected.
    /// A fact about the request or environment, not a wrapped cause -- there is no error
    /// object behind "this id does not exist".
    #[error("invalid capture source: {0}")]
    InvalidSource(String),

    /// The numeric half of a `monitor-<n>`/`window-<n>` source id did not parse as a `u32`.
    /// Split out of the `InvalidSource` family because this one case has a real, trivially
    /// preservable cause: `std::num::ParseIntError` is a core type, so unlike the
    /// backend errors above it needs no dependency-boundary workaround.
    #[error("invalid source id {source_id:?}: {cause}")]
    MalformedSourceId {
        source_id: String,
        #[source]
        cause: std::num::ParseIntError,
    },
}

pub type Result<T> = std::result::Result<T, ElementiumError>;

/// Implemented by an error enum whose variants also cross the Tauri IPC boundary as a
/// coded JSON envelope.
///
/// The envelope is `{"code": ..., "message": ..., "id": ...}`, for a command whose caller
/// (the WebRTC/media shim) needs to act differently depending on which variant occurred --
/// not merely log the message and give up.
///
/// `ipc_code` is matched on the variant, **never** derived from `self.to_string()` or any
/// other rendering of the message: the frontend branches on `code`, so a reworded message
/// must never change it. See `src-tauri/src/commands/mod.rs`'s `IpcErrorEnvelope` for where
/// this is consumed, and `specs/BACKLOG-2026-08-09-errors.md` item X2 for why it exists --
/// the 2026-08-09 outage was, in the end, a case of the page being unable to tell an error
/// it had to act on from one it should ignore.
///
/// Every implementor documents its own codes beside its `match`, because that is where
/// someone adding or renaming a variant will actually see them (a central registry a
/// contributor never opens is worse than none).
pub trait IpcErrorCode {
    /// A stable, `snake_case` identifier naming which variant this is. Never derived from
    /// `Display`/`to_string()` -- see the trait's own doc.
    fn ipc_code(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::ElementiumError;

    /// Pins the split of the old `ScreenCapture(String)`: a backend failure must carry a
    /// real, walkable source (type-erased, per this module's doc, but not stringified) --
    /// `source()` must reach the actual boxed error, not `None`.
    #[test]
    fn backend_failure_carries_its_boxed_cause_as_a_walkable_source() {
        let cause = std::io::Error::other("xcap blew up");
        let err = ElementiumError::Backend {
            description: "X11 capture backend (xcap) failed: xcap blew up".to_owned(),
            cause: Box::new(cause),
        };
        assert!(matches!(err, ElementiumError::Backend { .. }));
        assert!(
            std::error::Error::source(&err).is_some(),
            "Backend must preserve its boxed cause via #[source], not swallow it"
        );
    }

    /// New variant: a source id/selection that names nothing real (bad prefix, unknown id,
    /// no stream selected) has no error object behind it -- `reason` is the whole fact.
    #[test]
    fn invalid_source_has_no_underlying_source() {
        let err = ElementiumError::InvalidSource("no monitor with id 4 was found".to_owned());
        assert!(matches!(err, ElementiumError::InvalidSource(_)));
        assert!(std::error::Error::source(&err).is_none());
    }

    /// New variant: split out of `InvalidSource` because this one case has a real cause
    /// (`std::num::ParseIntError`) that costs nothing to preserve -- it is a core type, not
    /// one of the heavy backend dependencies `Backend`'s boxing works around.
    #[test]
    #[allow(clippy::expect_used)] // test-only: failing the parse is the point of this fixture
    fn malformed_source_id_carries_the_parse_error_as_its_source() {
        let cause = "not-a-number"
            .parse::<u32>()
            .expect_err("must fail to parse");
        let err = ElementiumError::MalformedSourceId {
            source_id: "monitor-not-a-number".to_owned(),
            cause,
        };
        assert!(matches!(err, ElementiumError::MalformedSourceId { .. }));
        assert!(std::error::Error::source(&err).is_some());
        assert!(err.to_string().contains("monitor-not-a-number"));
    }
}
