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

/// Which operation sharing the [`MediaWriteError`] failure surface produced it.
///
/// `send_mid_for` (the mid-resolution helper both writers call first) is deliberately *not*
/// a third `ResolveMid` variant here. It has exactly two callers -- `write_audio` and
/// `write_video` -- and nothing else ever calls it, so its failure is always really "the
/// audio write failed to resolve a mid" or "the video write failed to resolve a mid". A log
/// line reading `write_video: no mid published for track camera` tells a screen-share
/// dropout apart from a camera one immediately; `resolve_mid: no mid published for track
/// camera` would still leave the reader guessing which write called it before they even know
/// whether the picture or the voice was the casualty. So the caller's own operation is
/// threaded straight through instead of being replaced by a third one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaWriteOperation {
    /// [`crate::peer_connection::write_audio`], including its `send_mid_for` step.
    WriteAudio,
    /// [`crate::peer_connection::write_video`], including its `send_mid_for` step.
    WriteVideo,
}

impl std::fmt::Display for MediaWriteOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::WriteAudio => "write_audio",
            Self::WriteVideo => "write_video",
        })
    }
}

/// Failure surface shared by `write_audio` and `write_video` (and the `send_mid_for` helper
/// both call).
///
/// Sharing is permitted here under constitution principle I only because every caller of
/// `write_audio`/`write_video` handles every `kind` identically: drop the frame, log, and
/// keep the connection running. What sharing must not do is lose which of the two produced
/// the error -- `RTP write failed` on its own does not say whether a fifth of a second of
/// speech or an unmissed video frame was dropped -- so `operation` carries that alongside
/// `kind`, which is otherwise unchanged from before this type gained the field.
#[derive(Debug, Error)]
#[error("{operation}: {kind}")]
pub struct MediaWriteError {
    pub operation: MediaWriteOperation,
    #[source]
    pub kind: MediaWriteErrorKind,
}

/// The distinct ways a media write can fail, independent of which operation hit them.
///
/// Kept exactly as before `MediaWriteError` gained its `operation` field: callers that
/// matched on these variants keep matching on them unchanged.
#[derive(Debug, Error)]
pub enum MediaWriteErrorKind {
    /// The write named one of our own tracks that this connection has not published.
    #[error("no mid published for track {0}")]
    TrackNotPublished(elementium_types::MediaTrackKey),
    /// No local writer exists yet for this media kind (e.g. no transceiver of that kind was
    /// ever negotiated).
    ///
    /// `operation` on the enclosing [`MediaWriteError`] is still worth carrying here even
    /// though `str0m::media::MediaKind` already says audio-or-video: the two are not
    /// interchangeable once other variants (`TrackNotPublished`, `CodecNotNegotiated`,
    /// `Rtp`) are considered, none of which carry a kind of their own, and a single struct
    /// shape that always has `operation` is simpler to log and to match on than one field
    /// that exists on some variants and not others.
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

/// Codes for [`DataChannelWriteError`], read by the frontend's `RTCDataChannel.send()` shim
/// (`webrtc-shim.ts`) to tell "retry once `onopen` fires" from "this channel is dead" from
/// "the write itself failed" -- three conditions with three different right responses, which
/// is exactly what the constitution's "act on it or ignore it" test asks for. `NotOpenYet`
/// and `NoStream` both correspond to the DOM's own `InvalidStateError` for
/// `RTCDataChannel.send()` on a channel whose `readyState` is not `"open"` (WebRTC 1.0
/// §6.2); `Sctp` has no DOM analogue evidenced in the shipped bundle (nothing there catches
/// a `send()` failure by name -- see X2's investigation), so it reaches the page as a
/// generic rejection rather than a guessed exception name.
impl elementium_types::IpcErrorCode for DataChannelWriteError {
    fn ipc_code(&self) -> &'static str {
        match self {
            Self::NotOpenYet(_) => "data_channel_not_open",
            Self::NoStream(_) => "data_channel_no_stream",
            Self::Sctp { .. } => "data_channel_write_failed",
        }
    }
}

/// Which of the three call sites sharing the [`IoLoopError`] failure surface produced it.
///
/// Not one variant per function: `recv_and_feed` is itself called from two different
/// places with two different meanings -- once directly, waiting for the next datagram, and
/// again in a tight loop from `drain_backlog`, pulling datagrams already queued behind a
/// burst. A failure partway through draining a backlog (kernel buffer already had several
/// packets queued) is a different situation from a failure on the very first read of an
/// idle connection, so that distinction is carried here rather than collapsed into a single
/// `Receive` that could mean either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoLoopOperation {
    /// [`crate::peer_connection::poll_once`] draining str0m's output queue or handing it a
    /// timeout tick.
    PollOnce,
    /// [`crate::peer_connection::recv_and_feed`] called directly, waiting for the next UDP
    /// datagram.
    Receive,
    /// [`crate::peer_connection::recv_and_feed`] called repeatedly by
    /// [`crate::peer_connection::drain_backlog`] to pull further datagrams already queued in
    /// the kernel receive buffer.
    DrainBacklog,
}

impl std::fmt::Display for IoLoopOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::PollOnce => "poll_once",
            Self::Receive => "recv_and_feed",
            Self::DrainBacklog => "drain_backlog",
        })
    }
}

/// Failure surface shared by `poll_once`, `recv_and_feed` and the backlog drain they share.
///
/// Sharing is permitted here under constitution principle I: all three are the same io loop
/// at different points in one iteration, and a caller who cannot advance the loop for one
/// reason cannot advance it for any of the others either -- there is nothing to distinguish
/// by *in how the caller reacts*. But "socket error" alone does not say whether that was the
/// very first read on an idle connection or the middle of draining a burst, and those read
/// very differently in a log, so `operation` carries the call site while `kind` (unchanged
/// from before this type gained the field) carries the failure itself.
#[derive(Debug, Error)]
#[error("{operation}: {kind}")]
pub struct IoLoopError {
    pub operation: IoLoopOperation,
    #[source]
    pub kind: IoLoopErrorKind,
}

/// The distinct ways the io loop can fail, independent of which call site hit them.
#[derive(Debug, Error)]
pub enum IoLoopErrorKind {
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

/// Which UDP socket a [`SocketSetupError`] was setting up.
///
/// Shared by [`crate::engine::WebRtcEngine::create_connection`] (one socket per
/// connection) and [`crate::livekit::transport::Transport::new_with_e2ee`] (one socket
/// each for the publisher and subscriber `PeerConnection`s). All three bind and inspect a
/// `UdpSocket` the same way and every caller reacts to a failure the same way (log and
/// propagate), so constitution principle I permits sharing the type -- but "socket setup
/// failed" alone would not say which of up to three sockets a single `Transport` owns
/// failed, so `role` carries that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketRole {
    /// [`crate::engine::WebRtcEngine::create_connection`]'s single socket.
    Connection,
    /// The `LiveKit` publisher `PeerConnection`'s socket.
    Publisher,
    /// The `LiveKit` subscriber `PeerConnection`'s socket.
    Subscriber,
}

impl std::fmt::Display for SocketRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Connection => "connection",
            Self::Publisher => "publisher",
            Self::Subscriber => "subscriber",
        })
    }
}

/// Which step of setting up a UDP socket failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketSetupStep {
    /// `UdpSocket::bind` itself failed (e.g. the address is already in use).
    Bind,
    /// `bind` succeeded but `UdpSocket::local_addr` failed to report the address bound.
    LocalAddr,
}

impl std::fmt::Display for SocketSetupStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Bind => "bind",
            Self::LocalAddr => "local address lookup",
        })
    }
}

/// Failure surface shared by every UDP socket a connection or transport sets up. See
/// [`SocketRole`] for why sharing is permitted here.
#[derive(Debug, Error)]
#[error("{role} socket {step} failed")]
pub struct SocketSetupError {
    pub role: SocketRole,
    pub step: SocketSetupStep,
    #[source]
    pub source: std::io::Error,
}

/// Failure surface: [`crate::livekit::transport::Transport::set_subscriber_offer`].
#[derive(Debug, Error)]
pub enum SubscriberOfferError {
    /// `set_remote_description` accepted the SFU's offer but produced no answer. X6
    /// (`specs/BACKLOG-2026-08-09-errors.md`): this is *not* the "no pending offer"
    /// condition that caused the 2026-08-09 outage -- an `Offer` always yields
    /// `Ok(Some(answer))` from `set_remote_description`, so `Ok(None)` reaching here would
    /// itself be a bug in that invariant, worth surfacing as a hard error rather than
    /// silently treating the subscription as complete when it never produced anything to
    /// send back to the SFU.
    #[error("SFU offer produced no answer")]
    NoAnswerProduced,
}

/// Which room operation sharing the [`SignalingError`] failure surface produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalingOperation {
    /// [`crate::livekit::room::LiveKitRoom::connect`] failed to reach the SFU at all.
    Connect,
    /// [`crate::livekit::room::LiveKitRoom`]'s renegotiation path failed to send a
    /// publisher offer.
    SendPublisherOffer,
    /// Publishing a track failed to send its `AddTrack` request.
    SendAddTrack,
    /// [`crate::livekit::room::LiveKitRoom::set_track_muted`] failed to send a `Mute`
    /// request.
    SendMute,
}

impl std::fmt::Display for SignalingOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Connect => "connect",
            Self::SendPublisherOffer => "send_publisher_offer",
            Self::SendAddTrack => "publish_track",
            Self::SendMute => "set_track_muted",
        })
    }
}

/// Failure surface shared by every place `LiveKitRoom` talks to the SFU's signaling
/// channel (connect, and every `SignalSender::send`).
///
/// Sharing is permitted under constitution principle I: every one of these is "the
/// signaling channel to the SFU did not accept this", every caller's reaction is to log
/// and propagate the failure, and `operation` keeps a log line naming which of the four
/// distinct things that were being attempted actually failed -- exactly the field the
/// constitution's amendment on 2026-08-09 requires of a shared surface.
#[derive(Debug, Error)]
#[error("{operation}: signaling failed")]
pub struct SignalingError {
    pub operation: SignalingOperation,
    #[source]
    pub source: crate::livekit::signaling::SignalError,
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
    /// ICE candidate parsing/application failed.
    #[error("ICE candidate error: {0}")]
    IceCandidate(String),
    /// A UDP socket for a connection or transport could not be bound or inspected.
    #[error(transparent)]
    Socket(#[from] SocketSetupError),
    /// [`crate::livekit::transport::Transport::set_subscriber_offer`] failed.
    #[error(transparent)]
    Sdp(#[from] SubscriberOfferError),
    /// `LiveKit` signaling (WebSocket) failure.
    #[error(transparent)]
    Signaling(#[from] SignalingError),
}

/// Lets `?` keep working unchanged at every `Result<_, String>` Tauri command boundary
/// that calls into this crate -- see the module doc for why this is intentional, not a
/// leaky abstraction.
impl From<WebRtcError> for String {
    fn from(e: WebRtcError) -> Self {
        e.to_string()
    }
}
