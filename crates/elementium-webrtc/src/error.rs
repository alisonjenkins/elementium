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

/// Failure surface: [`crate::peer_connection::create_offer`].
///
/// One variant: the only way `create_offer` can fail is a caller-contract violation, not a
/// runtime condition, so there is no source to carry.
#[derive(Debug, Error)]
pub enum CreateOfferError {
    /// Nothing was requested (no new data channels, no new transceivers) and no previous
    /// offer exists to repeat. A genuine fault, not the "re-offer changes nothing" case --
    /// that one has a previous offer to hand back and is not an error at all.
    #[error("no changes to offer, and no previous offer to repeat")]
    NothingToOffer,
}

/// Failure surface: [`crate::peer_connection::create_answer`].
#[derive(Debug, Error)]
pub enum CreateAnswerError {
    /// `createAnswer()` was called before `set_remote_description` cached an answer for a
    /// remote offer. Mirrors the DOM's own `InvalidStateError` for the same misuse.
    #[error("no remote offer has been set; call set_remote_description(offer) first")]
    NoRemoteOffer,
}

/// Failure surface: [`crate::peer_connection::set_remote_description`].
///
/// Deliberately has **no** "no pending offer" variant: an answer arriving with nothing
/// pending is not a fault (see `set_remote_description`'s doc comment) and stays `Ok(None)`.
/// Reintroducing that condition here as an error is the exact regression that froze
/// signalling and tore every call down every fifteen seconds on 2026-08-09.
#[derive(Debug, Error)]
pub enum SetRemoteDescriptionError {
    /// The remote offer's SDP did not parse.
    #[error("invalid offer SDP")]
    OfferParse(#[source] str0m::error::SdpError),
    /// The remote answer's SDP did not parse.
    #[error("invalid answer SDP")]
    AnswerParse(#[source] str0m::error::SdpError),
    /// str0m parsed the offer but refused to accept it.
    #[error("str0m rejected the remote offer")]
    OfferRejected(#[source] str0m::RtcError),
    /// str0m parsed the answer but refused to accept it against the pending offer.
    #[error("str0m rejected the remote answer")]
    AnswerRejected(#[source] str0m::RtcError),
}

/// Failure surface: [`crate::peer_connection::add_ice_candidate`].
#[derive(Debug, Error)]
pub enum AddIceCandidateError {
    /// The candidate string did not parse as ICE candidate SDP.
    ///
    /// Deliberately does not carry the candidate string itself: candidates sit next to ICE
    /// credential material in the same signalling payloads the constitution (principle IV)
    /// forbids logging. `len` and the parse error are enough to diagnose a malformed
    /// candidate without risking that material reaching a log.
    #[error("malformed ICE candidate ({len} bytes)")]
    Malformed {
        len: usize,
        #[source]
        source: str0m::error::IceError,
    },
}

/// Failure surface shared by `write_audio`, `write_video` and `send_mid_for`.
///
/// Sharing is permitted here under constitution principle I only because every caller of
/// `write_audio`/`write_video` handles every variant identically: drop the frame, log, and
/// keep the connection running. A caller that needed to react differently per variant would
/// need its own enum instead.
#[derive(Debug, Error)]
pub enum MediaWriteError {
    /// The write named one of our own tracks that this connection has not published.
    #[error("no mid published for track {0}")]
    TrackNotPublished(elementium_types::MediaTrackKey),
    /// No local writer exists yet for this media kind (e.g. no transceiver of that kind was
    /// ever negotiated).
    #[error("no {0:?} writer available")]
    NoWriter(str0m::media::MediaKind),
    /// The mid has a writer, but the far end never negotiated a payload type for this
    /// codec.
    #[error("no {codec} payload type negotiated")]
    CodecNotNegotiated { codec: &'static str },
    /// str0m rejected the RTP write itself.
    #[error("RTP write failed")]
    Rtp(#[source] str0m::RtcError),
}

/// Failure surface: [`crate::peer_connection::write_data_channel`].
#[derive(Debug, Error)]
pub enum DataChannelWriteError {
    /// No channel named `label` has completed its DCEP handshake yet. Transient: the
    /// caller should retry after `onopen` fires for it.
    #[error("data channel '{0}' has not completed its handshake yet")]
    NotOpenYet(String),
    /// A channel named `label` did complete its handshake once, but str0m has no writable
    /// SCTP stream for it now (closed, locally or by the remote peer). Not transient: this
    /// channel is dead and will not become writable again.
    #[error("data channel '{0}' has no writable SCTP stream (closed or not yet open)")]
    NoStream(String),
    /// str0m's SCTP layer rejected the write outright.
    #[error("SCTP write to data channel '{label}' failed")]
    Sctp {
        label: String,
        #[source]
        source: str0m::RtcError,
    },
}

/// Failure surface shared by `poll_once`, `recv_and_feed` and the backlog drain they share.
///
/// Sharing is permitted here under constitution principle I: all three are the same io loop
/// at different points in one iteration, and a caller who cannot advance the loop for one
/// reason cannot advance it for any of the others either -- there is nothing to distinguish
/// by at the call site.
#[derive(Debug, Error)]
pub enum IoLoopError {
    /// `Rtc::poll_output` reported a fatal error. The connection is no longer usable;
    /// callers set `pc.alive = false` alongside this.
    #[error("str0m reported a fatal error")]
    Str0mFatal(#[source] str0m::RtcError),
    /// Handing input (a timeout tick or a received datagram) to str0m failed.
    #[error("failed to hand input to str0m")]
    Input(#[source] str0m::RtcError),
    /// A socket operation (read-timeout configuration, `recv_from`, `local_addr`) failed.
    #[error("socket error")]
    Socket(#[source] std::io::Error),
}

/// Errors from the native WebRTC / `LiveKit` transport layer.
#[derive(Debug, Error)]
pub enum WebRtcError {
    /// [`create_offer`](crate::peer_connection::create_offer) had nothing to describe.
    #[error(transparent)]
    CreateOffer(#[from] CreateOfferError),
    /// [`create_answer`](crate::peer_connection::create_answer) was called with no remote
    /// offer in force.
    #[error(transparent)]
    CreateAnswer(#[from] CreateAnswerError),
    /// [`set_remote_description`](crate::peer_connection::set_remote_description) failed to
    /// parse or apply the remote SDP.
    #[error(transparent)]
    SetRemoteDescription(#[from] SetRemoteDescriptionError),
    /// [`add_ice_candidate`](crate::peer_connection::add_ice_candidate) failed to parse the
    /// candidate.
    #[error(transparent)]
    AddIceCandidate(#[from] AddIceCandidateError),
    /// [`write_audio`](crate::peer_connection::write_audio)/
    /// [`write_video`](crate::peer_connection::write_video) failed to write RTP.
    #[error(transparent)]
    MediaWrite(#[from] MediaWriteError),
    /// [`write_data_channel`](crate::peer_connection::write_data_channel) failed to write
    /// SCTP.
    #[error(transparent)]
    DataChannelWrite(#[from] DataChannelWriteError),
    /// The io loop (`poll_once`/`recv_and_feed`/backlog drain) failed.
    #[error(transparent)]
    IoLoop(#[from] IoLoopError),
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
