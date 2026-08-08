//! Structured errors for the native WebRTC / `LiveKit` transport layer.
//!
//! Replaces the `Result<_, String>` that used to be threaded through
//! `peer_connection.rs`/`engine.rs`/`livekit/*.rs`: callers inside this crate can now
//! match on a real error kind (lock poisoning, an unknown track kind, a closed channel,
//! ...) instead of parsing formatted text. The Tauri command layer still needs a plain
//! `String` for its IPC boundary (`Result<T, String>` is what `#[tauri::command]`
//! serializes), so `From<WebRtcError> for String` makes `?` keep working unchanged at
//! every existing call site there -- this is purely an internal type-safety upgrade,
//! not a breaking change to the command surface.

use thiserror::Error;

/// Errors from the native WebRTC / `LiveKit` transport layer.
#[derive(Debug, Error)]
pub enum WebRtcError {
    /// A peer-connection or transport mutex was poisoned by a panic in another thread.
    #[error("lock is poisoned")]
    LockPoisoned,
    /// No local writer/mid exists for the given media kind (e.g. `write_audio` called
    /// before any audio transceiver was negotiated).
    #[error("no {0} writer/mid available")]
    NoWriterForKind(&'static str),
    /// A write named one of our own tracks that this connection has not published.
    ///
    /// Distinct from [`WebRtcError::NoWriterForKind`] on purpose: that one means the media
    /// kind was never negotiated at all, this one means the kind exists but the specific
    /// track does not. Conflating them is what a by-kind fallback would do, and it would
    /// send a screen share down the camera's m-line, which every receiver silently fails to
    /// decode.
    #[error("no mid published for track {0}")]
    NoMidForTrack(String),
    /// A track kind string from a caller (JS, signaling) didn't match a known kind.
    #[error("unknown track kind: {0}")]
    UnknownTrackKind(String),
    /// A signaling/command channel was closed while still expected to be open.
    #[error("channel closed: {0}")]
    ChannelClosed(&'static str),
    /// Timed out waiting for an expected response.
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
    /// SDP offer/answer construction or application failed.
    #[error("SDP error: {0}")]
    Sdp(String),
    /// ICE candidate parsing/application failed.
    #[error("ICE candidate error: {0}")]
    IceCandidate(String),
    /// Socket bind/IO failure.
    #[error("socket error: {0}")]
    Socket(String),
    /// `LiveKit` signaling (WebSocket) failure.
    #[error("signaling error: {0}")]
    Signaling(String),
    /// Any other internal failure not worth a dedicated variant (mostly wrapped
    /// `str0m`/FFI error text) -- still a real error value, just not one callers
    /// currently need to distinguish by kind.
    #[error("{0}")]
    Other(String),
}

impl WebRtcError {
    /// Build an [`WebRtcError::Other`] from any displayable error/message.
    pub fn other(msg: impl std::fmt::Display) -> Self {
        Self::Other(msg.to_string())
    }
}

/// Lets the many pre-existing `Err("literal")`/`.ok_or("literal")?` call sites in this
/// crate keep compiling unchanged after their surrounding functions were retyped from
/// `Result<_, String>` to `Result<_, WebRtcError>` -- each such literal is a genuine,
/// specific error message that just hasn't been promoted to its own variant yet.
impl From<&str> for WebRtcError {
    fn from(s: &str) -> Self {
        Self::Other(s.to_string())
    }
}

/// Same rationale as `From<&str>`, for the `format!(...)`-built error strings.
impl From<String> for WebRtcError {
    fn from(s: String) -> Self {
        Self::Other(s)
    }
}

/// Lets `?` keep working unchanged at every `Result<_, String>` Tauri command boundary
/// that calls into this crate -- see the module doc for why this is intentional, not a
/// leaky abstraction.
impl From<WebRtcError> for String {
    fn from(e: WebRtcError) -> Self {
        e.to_string()
    }
}
