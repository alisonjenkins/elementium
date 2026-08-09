use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::channel::{ChannelConfig, ChannelId, Reliability};
use str0m::format::Codec;
use str0m::media::{Direction, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};

use elementium_types::{
    IceConnectionState as IceState, MediaTrackKey, PeerConnectionState, PlaintextMedia, SdpType,
    SessionDescription, WireMedia,
};

/// Events emitted by a peer connection to the application layer.
///
/// Generic over the media payload so the encryption boundary is visible in the type:
/// [`WirePcEvent`] (straight off the network, possibly ciphertext) is produced by
/// `poll_once`, and only [`crate::e2ee_io::maybe_decrypt_event`] can turn it into a
/// `PcEvent<PlaintextMedia>`, which is the only thing the decode pipelines accept.
#[derive(Debug, Clone)]
pub enum PcEvent<P = PlaintextMedia> {
    /// ICE connection state changed.
    IceConnectionStateChange(IceState),
    /// Peer connection state changed.
    ConnectionStateChange(PeerConnectionState),
    /// A local ICE candidate was gathered.
    IceCandidate(String),
    /// ICE gathering completed (null candidate).
    IceGatheringComplete,
    /// DTLS+ICE connected, media can flow.
    Connected,
    /// A new remote media track was added.
    RemoteTrackAdded { mid: String, kind: String },
    /// A receiver cannot decode our video and is asking for a keyframe (RTCP PLI/FIR).
    ///
    /// Must reach the encoder: until it produces a keyframe the receiver has nothing it
    /// can decode, and it will keep asking. A log of repeating requests with no keyframe
    /// in between is precisely what a corrupted remote picture looks like from our side.
    KeyframeRequested { mid: String },
    /// Received audio data (Opus packet) on a specific remote track.
    ///
    /// `mid` identifies which media line this packet came from -- required because a
    /// single `PeerConnection` can carry more than one remote audio track (e.g. two
    /// participants' audio bundled onto one connection in a mesh call). Without it,
    /// packets from independent Opus encoder streams would be indistinguishable and
    /// have to be fed through one shared decoder, corrupting both streams (this was a
    /// real bug: interleaving two encoders' packets into a single stateful Opus decoder
    /// produces garbage audio, heard as "digital screeching").
    ///
    /// `contiguous` is str0m's own gap detector (`MediaData::contiguous`): `false` means
    /// one or more RTP packets were lost between this frame and the last one emitted for
    /// this track. Opus is stateful (LPC history) -- feeding it a real packet right after
    /// an unconcealed gap, as if no time had passed, produces a phase discontinuity at
    /// that boundary. Under sustained loss, repeated unconcealed discontinuities produce
    /// harsh broadband noise bursts audible as "digital screeching", not just an
    /// occasional click. Carried through so the playback pipeline can call Opus's
    /// packet-loss concealment for the gap before decoding the real packet.
    AudioData {
        mid: String,
        data: P,
        contiguous: bool,
    },
    /// Received video data on a specific remote track. See `AudioData` for why `mid` is
    /// required.
    ///
    /// `codec` travels with the packet because the receive path cannot infer it. A remote
    /// track's codec is whatever that peer chose to publish, so two tracks on one
    /// connection can differ, and a decoder built for the wrong one produces nothing while
    /// reporting nothing. It used to be assumed VP8 everywhere.
    VideoData {
        mid: String,
        data: P,
        codec: elementium_codec::VideoCodec,
    },
    /// Aggregated outbound stats for one of our sending tracks, from RTCP receiver
    /// reports sent back by the peers.
    ///
    /// `loss` is the fraction (`0.0..=1.0`) of our packets the peers report not
    /// receiving, and is `None` when no report arrived in the interval. It is the only
    /// direct measurement we have of how much of our audio is actually being lost, which
    /// is what decides how much FEC redundancy the Opus encoder should emit.
    EgressStats {
        mid: String,
        loss: Option<f32>,
        rtt_ms: Option<u64>,
        packets: u64,
        nacks: u64,
    },
}

impl<P> PcEvent<P> {
    /// Is this a high-rate media payload that is safe to drop under backpressure?
    ///
    /// `AudioData`/`VideoData` are the only variants that arrive at a rate that can
    /// realistically fill a bounded channel, and a dropped one is inherently self-healing:
    /// the next frame supersedes it and, for audio, PLC covers the gap. Everything else --
    /// state transitions, ICE candidates, stats, keyframe requests -- is comparatively rare
    /// and each instance is the only notice of something that will not repeat, so those
    /// belong on an unbounded queue instead. See `engine::io_loop`, which routes on this.
    #[must_use]
    pub const fn is_media(&self) -> bool {
        matches!(self, Self::AudioData { .. } | Self::VideoData { .. })
    }
}

/// A [`PcEvent`] whose media payload came straight off the network and may still be
/// encrypted. Cannot reach a decoder: decoders only accept [`PlaintextMedia`].
pub type WirePcEvent = PcEvent<WireMedia>;

/// One data-channel event, queued for the Tauri command layer to drain and forward to
/// the page.
///
/// Deliberately not folded into [`PcEvent`], even though both ultimately reach the
/// frontend. `PcEvent<P>` exists to make the E2EE boundary visible in the type, and every
/// `WirePcEvent` the I/O loop produces is pushed through
/// [`crate::e2ee_io::maybe_decrypt_event`], whose `passthrough` match is written out
/// arm-by-arm on purpose so the compiler catches a forgotten variant. Data-channel bytes
/// are SCTP payload, not RTP media -- there is no decoder to feed and nothing for that
/// boundary to mean for them. Routing them through it anyway would mean either silently
/// dropping them in `passthrough` (repeating the exact bug this feature exists to fix, one
/// layer further down) or teaching that unrelated module about data channels for a
/// distinction that does not apply to them.
#[derive(Debug, Clone)]
pub enum DataChannelEvent {
    /// The DCEP handshake for the channel named `label` completed; str0m will now hand
    /// back a writable [`str0m::channel::Channel`] for it (see
    /// [`PeerConnectionInner::data_channels`] for why not sooner).
    Open { label: String },
    /// A message arrived on the channel named `label`. `binary` distinguishes the DOM
    /// `ArrayBuffer` case from the string case, which `RTCDataChannel.onmessage` must
    /// reconstruct differently.
    Message {
        label: String,
        binary: bool,
        data: Vec<u8>,
    },
    /// The channel named `label` closed, locally or by the remote peer.
    Close { label: String },
}

/// Shared state for a single peer connection.
pub struct PeerConnectionInner {
    pub id: String,
    pub rtc: Rtc,
    /// Set when the page has called `restartIce()` and the next offer should carry fresh
    /// ICE credentials.
    ///
    /// A flag rather than an immediate action, because that is what the DOM API means: it
    /// marks the connection as needing renegotiation, and the offer the page then makes is
    /// where the restart belongs. Without it, a network change -- a laptop waking, Wi-Fi to
    /// Ethernet -- left the call unable to recover short of a full reconnect.
    pub ice_restart_requested: bool,
    /// The mid we *send* audio on: the first audio transceiver offered.
    ///
    /// Not "the audio m-line". A connection can carry several, and since livekit protocol
    /// 17 it usually does -- one `recvonly` section per remote participant, on this same
    /// connection. Only the first is ours to write to.
    pub audio_mid: Option<Mid>,
    /// The mid we *send* video on. See [`PeerConnectionInner::audio_mid`].
    pub video_mid: Option<Mid>,
    /// The mid each of our own tracks is published on.
    ///
    /// `audio_mid` and `video_mid` cannot answer this once a participant sends two video
    /// tracks -- their camera and their screen -- because both are video and there is only
    /// one slot. Writing the second to the first's mid is accepted by the SFU, advances the
    /// sender's counters, and is never decodable by anyone: a failure indistinguishable
    /// from success from this side. Hence an explicit map, and no by-kind fallback anywhere
    /// that reads it.
    pub send_mids: HashMap<MediaTrackKey, Mid>,
    /// `(kind, track_id)` of every transceiver already offered, to recognise a re-offer.
    ///
    /// Keyed on the track rather than on the kind. A caller that re-offers a transceiver
    /// it already has must not be given a second m-line for it; a caller that asks for
    /// another transceiver of the same kind must be. Keying on the kind alone cannot tell
    /// those apart, and answered the second case with the first -- which is how a client
    /// asking for three receive sections got one, and heard nobody.
    ///
    /// A transceiver with no track is always new: `recvonly` placeholders have no track by
    /// construction, and there is nothing to distinguish two of them by.
    pub offered_tracks: Vec<(MediaKind, String)>,
    pub pending_offer: Option<SdpPendingOffer>,
    /// The last offer this connection produced, returned again when a re-offer would change
    /// nothing. See `create_offer`.
    pub last_offer_sdp: Option<String>,
    pub remote_mids: HashMap<Mid, MediaKind>,
    pub audio_frame_count: u64,
    /// Wall-clock instant corresponding to audio RTP timestamp 0 on this connection.
    ///
    /// Set on the first audio write; every later frame's wallclock is derived from it and
    /// the frame counter, rather than read from the clock at write time. See
    /// [`audio_wallclock`].
    pub audio_epoch: Option<Instant>,
    pub video_frame_count: u64,
    /// Instant corresponding to video RTP timestamp 0 on this connection.
    ///
    /// Video timestamps are measured from this rather than counted, because the capture
    /// rate is neither fixed nor known in advance -- see [`write_video`].
    pub video_epoch: Option<Instant>,
    /// Last RTP sequence number emitted per audio `mid`, for out-of-order detection.
    ///
    /// Opus frames must reach the decoder in the order they were encoded. If str0m emits
    /// them out of sequence, every individual packet still decodes cleanly (no errors,
    /// plausible amplitudes) but the audio is scrambled in time -- which sounds like
    /// harsh garbage and is invisible to every other check in this pipeline. Captured
    /// here so ordering is provable from logs instead of inferred.
    pub last_audio_seq: HashMap<Mid, u64>,
    pub alive: bool,
    /// When ICE first reported `Disconnected` without having recovered since.
    ///
    /// str0m's `Disconnected` is explicitly *not* terminal -- its own docs say it "may
    /// trigger intermittently and resolve just as spontaneously on less reliable networks",
    /// and that str0m has no `failed`/`closed` state at all because "it's always possible
    /// to come back". Treating it as fatal tore the connection down on the first transient
    /// blip and never recovered. Media stops instantly; whether the user notices depends
    /// only on how quickly something above rejoins.
    pub ice_disconnected_since: Option<Instant>,
    /// Set to true after SDP negotiation triggers DTLS initialization inside str0m.
    /// Before this, calling `poll_output` would panic in dimpl because `handle_timeout`
    /// has not been propagated to the DTLS layer (str0m bug: `do_handle_timeout`
    /// doesn't call `dtls.handle_timeout`, only `init_dtls` does).
    pub dtls_initialized: bool,
    /// Cached answer SDP generated by `set_remote_description(Offer)`.
    /// str0m generates the answer during `accept_offer`, but the WebRTC API expects
    /// `createAnswer()` to return it separately. Cache it here for that call.
    pub cached_answer: Option<SessionDescription>,
    /// Negotiated Opus payload type, recorded on each audio write.
    ///
    /// Needed to pick our own audio packets back out of the outgoing UDP stream at the
    /// socket, where audio, video, RTCP and STUN are all interleaved on one port.
    pub audio_pt: Option<u8>,
    /// Cadence of audio RTP packets as they actually leave the socket. See
    /// [`AudioSendPacing`].
    pub audio_send_pacing: AudioSendPacing,
    /// Counter for UDP transmit logging (throttle after first 10).
    pub transmit_log_count: u64,
    /// Counter for UDP receive logging (throttle after first 10).
    pub recv_log_count: u64,
    /// Cached remote ICE candidate SDP strings, re-added after SDP renegotiation.
    /// str0m's `accept_offer` may clear remote candidates, so we re-add them.
    pub remote_candidates: Vec<String>,
    /// Latest outbound (egress) RTCP stats per mid, from `Event::MediaEgressStats`.
    ///
    /// str0m only *emits* these on its own one-second cadence (see
    /// `set_stats_interval` above); anything that wants "the current numbers" on demand,
    /// like `getStats()`, has nowhere else to read them from between events. Kept as a
    /// snapshot per mid, overwritten in place, rather than appended -- callers want the
    /// most recent measurement, not a history.
    pub egress_stats: HashMap<Mid, str0m::stats::MediaEgressStats>,
    /// Latest inbound (ingress) RTCP stats per mid, from `Event::MediaIngressStats`. See
    /// `egress_stats` for why this is a snapshot.
    pub ingress_stats: HashMap<Mid, str0m::stats::MediaIngressStats>,
    /// Latest connection-level stats (RTT, selected ICE candidate pair), from
    /// `Event::PeerStats`. See `egress_stats` for why this is a snapshot.
    pub peer_stats: Option<str0m::stats::PeerStats>,
    /// Label -> `ChannelId` of every data channel this side has seen open, i.e. every
    /// channel `Event::ChannelOpen` has fired for.
    ///
    /// Populated only from that event, on both the offering and answering side. str0m's
    /// own docs for `Rtc::channel()` say it returns `None` for a `ChannelId` fresh out of
    /// `SdpApi::add_channel_with_config()` -- "This is first available when a `ChannelId`
    /// is advertised via `Event::ChannelOpen`" -- so there is no shortcut that lets the
    /// side which called `add_channel_with_config` start writing immediately; it has to
    /// wait for the DCEP handshake to complete like the far end does.
    pub data_channels: HashMap<String, ChannelId>,
    /// Reverse of `data_channels`. `Event::ChannelData` and `Event::ChannelClose` carry
    /// only a `ChannelId`, not the label, so this is what turns one back into the name the
    /// application (and the shim's `RTCDataChannel` objects) actually key on.
    data_channel_labels: HashMap<ChannelId, String>,
    /// Data-channel events queued since the last drain, for the Tauri command layer to
    /// forward to the page. See [`DataChannelEvent`] for why this is a separate queue
    /// rather than more `PcEvent` variants.
    pub data_channel_events: Vec<DataChannelEvent>,
}

/// Thread-safe handle to a peer connection.
pub type PeerConnectionHandle = Arc<Mutex<PeerConnectionInner>>;

/// Create a new peer connection configured for audio and video.
#[must_use]
pub fn create_peer_connection(id: String) -> PeerConnectionInner {
    let now = Instant::now();
    let mut rtc = RtcConfig::new()
        .clear_codecs()
        .enable_opus(true)
        .enable_vp8(true)
        // H.264 as well as VP8. Not a preference between them -- an offer is not
        // per-direction, so offering H.264 also says we will *receive* it, and until there
        // was a decoder that was a promise we could not keep: a peer taking us up on it
        // became a black tile with nothing to explain it. `NegotiatedDecoder` handles H.264
        // now, and the receive path chooses the decoder and the E2EE framing from the codec
        // that actually arrived rather than assuming VP8.
        //
        // VP8 stays enabled and stays first. Every peer supports it, and this changes what
        // is *available*, not what is chosen.
        .enable_h264(true)
        // Off by default in str0m, which means `Event::MediaEgressStats` never fires and
        // the RTCP receiver reports the peers are already sending us are parsed and then
        // thrown away. Those reports carry the fraction of our packets each peer failed
        // to receive -- the only real measurement of outbound loss available to us, and
        // what the Opus encoder needs to size its FEC redundancy correctly instead of
        // guessing. One second matches the RTCP reporting interval; polling faster would
        // just re-report the same numbers.
        .set_stats_interval(Some(Duration::from_secs(1)))
        .build(now);

    // str0m's DTLS layer (dimpl) requires handle_timeout before any poll_output.
    // Set it immediately so no code path can hit poll_output with last_now=None.
    let _ = rtc.handle_input(Input::Timeout(now));

    PeerConnectionInner {
        id,
        ice_restart_requested: false,
        rtc,
        audio_mid: None,
        video_mid: None,
        send_mids: HashMap::new(),
        last_offer_sdp: None,
        offered_tracks: Vec::new(),
        pending_offer: None,
        remote_mids: HashMap::new(),
        audio_frame_count: 0,
        audio_epoch: None,
        video_epoch: None,
        video_frame_count: 0,
        last_audio_seq: HashMap::new(),
        alive: true,
        ice_disconnected_since: None,
        dtls_initialized: false,
        cached_answer: None,
        audio_pt: None,
        audio_send_pacing: AudioSendPacing::default(),
        transmit_log_count: 0,
        recv_log_count: 0,
        remote_candidates: Vec::new(),
        egress_stats: HashMap::new(),
        ingress_stats: HashMap::new(),
        peer_stats: None,
        data_channels: HashMap::new(),
        data_channel_labels: HashMap::new(),
        data_channel_events: Vec::new(),
    }
}

/// Every usable local address, most-likely-reachable first.
///
/// Loopback and link-local are excluded: a peer can never reach the first, and the second
/// needs a scope id to be routable.
///
/// NOTE: advertising all of these was tried and measurably made connectivity *worse* --
/// failures went from roughly half of runs to three quarters. Each extra candidate is
/// another pair for ICE to check, and `LiveKit`'s SFU gives a participant a fixed window to
/// connect before removing it entirely; spending that window probing Docker bridges and VPN
/// interfaces exhausts it. Kept because the enumeration is the right primitive for choosing
/// *better*, not for offering *more*.
#[must_use]
pub fn local_host_addresses() -> Vec<std::net::IpAddr> {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return get_local_ip().into_iter().collect();
    };
    ifaces
        .into_iter()
        .map(|i| i.ip())
        .filter(|ip| !ip.is_loopback() && !is_link_local(*ip))
        .collect()
}

const fn is_link_local(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_link_local(),
        std::net::IpAddr::V6(v6) => v6.is_unicast_link_local(),
    }
}

/// Add a local UDP host candidate for the address that routes to the internet.
pub fn add_local_candidate(pc: &mut PeerConnectionInner, addr: SocketAddr) {
    let addr = if addr.ip().is_unspecified() {
        let real_ip = get_local_ip().unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        SocketAddr::new(real_ip, addr.port())
    } else {
        addr
    };
    add_host_candidate(pc, addr);
}

fn add_host_candidate(pc: &mut PeerConnectionInner, addr: SocketAddr) {
    match Candidate::host(addr, "udp") {
        Ok(candidate) => {
            tracing::info!(pc_id = %pc.id, %addr, "Adding local ICE candidate");
            pc.rtc.add_local_candidate(candidate);
        }
        Err(e) => {
            tracing::error!(pc_id = %pc.id, %addr, err = %e, "Failed to create local candidate");
        }
    }
}

/// Add a server-reflexive (STUN-discovered public) candidate to the peer connection.
pub fn add_srflx_candidate(
    pc: &mut PeerConnectionInner,
    srflx_addr: SocketAddr,
    base_addr: SocketAddr,
) {
    match Candidate::server_reflexive(srflx_addr, base_addr, "udp") {
        Ok(candidate) => {
            tracing::info!(
                pc_id = %pc.id,
                srflx = %srflx_addr,
                base = %base_addr,
                "Adding srflx ICE candidate"
            );
            pc.rtc.add_local_candidate(candidate);
        }
        Err(e) => {
            tracing::error!(
                pc_id = %pc.id,
                srflx = %srflx_addr,
                base = %base_addr,
                err = %e,
                "Failed to create srflx candidate"
            );
        }
    }
}

/// Discover the local IP address by connecting a UDP socket to a public address.
#[must_use]
pub fn get_local_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    // Connect to a public IP (doesn't actually send anything)
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

/// Info about a data channel to include in the SDP offer.
#[derive(Debug, Clone)]
pub struct DataChannelInfo {
    pub label: String,
    pub ordered: bool,
    pub max_retransmits: Option<u16>,
    pub max_packet_life_time: Option<u16>,
    pub protocol: String,
}

/// Info about a transceiver to include in the SDP offer.
#[derive(Debug, Clone)]
pub struct TransceiverInfo {
    pub kind: MediaKind,
    pub direction: Direction,
    /// Track id to advertise as `a=msid:<stream> <track_id>`.
    ///
    /// `LiveKit`'s SFU associates a published track with an m-line by matching the `cid`
    /// from `AddTrackRequest` against the offer's msid track id -- livekit-client sets
    /// `cid` to the `MediaStreamTrack.id`, which *is* that value, so they agree by
    /// construction. Leaving this `None` lets str0m invent an id the SFU has never seen,
    /// and the association then depends on the server's by-kind fallback.
    pub track_id: Option<String>,
    /// Which of our own tracks this m-line carries, when it carries one of ours.
    ///
    /// `None` for a `recvonly` placeholder, which sends nothing and so has no send mid to
    /// register. Present for anything we publish, and it is what lets a later write find
    /// this m-line again rather than guessing by media kind.
    pub key: Option<MediaTrackKey>,
}

impl TransceiverInfo {
    /// Create from JS string values.
    ///
    /// The key is inferred from the kind because this path is the plain (non-`LiveKit`)
    /// peer connection, where a participant publishes at most one track of each kind. A
    /// screen share arrives through `LiveKit`, which names its tracks explicitly.
    #[must_use]
    pub fn from_js(
        kind: &str,
        direction: Option<&str>,
        track_id: Option<String>,
        source: Option<&str>,
    ) -> Self {
        let kind = match kind {
            "video" => MediaKind::Video,
            _ => MediaKind::Audio,
        };
        let direction = match direction {
            Some("sendonly") => Direction::SendOnly,
            Some("recvonly") => Direction::RecvOnly,
            Some("inactive") => Direction::Inactive,
            _ => Direction::SendRecv,
        };
        // Named by the caller where possible. Assuming the kind is enough was wrong the
        // moment a second video track existed: both the camera and the screen share
        // resolved to `video/camera`, the first m-line offered claimed it, and the other
        // track had no m-line of its own. The far end saw the camera in the screen-share
        // tile and nothing in the camera tile.
        let key = match direction {
            Direction::RecvOnly | Direction::Inactive => None,
            _ => Some(
                source
                    .and_then(|s| key_for_source(kind, s))
                    .unwrap_or_else(|| default_key_for(kind)),
            ),
        };
        Self {
            kind,
            direction,
            track_id,
            key,
        }
    }
}

/// The mid this connection publishes `key` on.
///
/// Deliberately without a by-kind fallback. Falling back to "the first video mid" for a
/// track that has not been published would write a screen share down the camera's m-line:
/// the SFU accepts the packets, the sender's counters advance, and no participant can
/// decode them. Refusing the write leaves an error at the point of the fault instead of a
/// silent black picture at every receiver.
fn send_mid_for(
    pc: &PeerConnectionInner,
    key: MediaTrackKey,
) -> Result<Mid, crate::error::WebRtcError> {
    pc.send_mids
        .get(&key)
        .copied()
        .ok_or_else(|| crate::error::WebRtcError::NoMidForTrack(key.to_string()))
}

/// The track a named source refers to, or `None` when the name is not one we publish.
///
/// Unrecognised names fall back to the by-kind default rather than inventing a key: a track
/// we cannot place is better sent on the default m-line than not at all, and the name comes
/// from the frontend, which can be older than this list.
fn key_for_source(kind: MediaKind, source: &str) -> Option<MediaTrackKey> {
    match (kind, source) {
        (MediaKind::Video, "camera") => Some(MediaTrackKey::camera()),
        (MediaKind::Video, "screen_share") => Some(MediaTrackKey::screen_share()),
        (MediaKind::Audio, "microphone") => Some(MediaTrackKey::microphone()),
        (MediaKind::Audio, "screen_share_audio") => Some(MediaTrackKey::screen_share_audio()),
        _ => None,
    }
}

/// The track a connection sends on the first m-line of a given kind, absent other
/// information.
///
/// Correct wherever only one track of each kind exists: the plain peer connection, and the
/// answerer role, where the m-lines come from the remote offer and carry no names of ours.
const fn default_key_for(kind: MediaKind) -> MediaTrackKey {
    match kind {
        MediaKind::Audio => MediaTrackKey::microphone(),
        MediaKind::Video => MediaTrackKey::camera(),
    }
}

/// Ask that the next offer carry fresh ICE credentials.
///
/// Mirrors `RTCPeerConnection.restartIce()`: it records the request and returns. The
/// restart is applied by [`create_offer`], because that is when an offer exists to carry
/// it, and the page is what decides to make one.
pub const fn request_ice_restart(pc: &mut PeerConnectionInner) {
    pc.ice_restart_requested = true;
}


/// Create an SDP offer with the specified data channels and transceivers.
///
/// Unlike the WebRTC browser API which implicitly includes all added transceivers
/// and data channels, this function takes explicit parameters because str0m's
/// SDP API builds the offer incrementally.
///
/// # Errors
///
/// Returns an error if str0m has no pending SDP changes to apply.
pub fn create_offer(
    pc: &mut PeerConnectionInner,
    data_channels: &[DataChannelInfo],
    transceivers: &[TransceiverInfo],
) -> Result<SessionDescription, crate::error::WebRtcError> {
    let mut api = pc.rtc.sdp_api();

    // A requested ICE restart is applied to *this* offer and then forgotten.
    //
    // `restartIce()` in the DOM does not renegotiate by itself: it marks the connection as
    // needing one and the page then makes an offer. Following that shape means the restart
    // rides on the offer the page was going to send anyway, rather than this layer inventing
    // a renegotiation nobody asked for.
    //
    // Local candidates are kept, because the usual reason for a restart -- a network change,
    // a laptop waking up -- invalidates the *path*, not every address this machine has, and
    // rediscovering them all costs seconds of silence for no benefit.
    if pc.ice_restart_requested {
        pc.ice_restart_requested = false;
        api.ice_restart(true);
        tracing::info!(pc_id = %pc.id, "applying a requested ICE restart to this offer");
    }

    // Add data channels (m=application)
    for dc in data_channels {
        let reliability = dc.max_retransmits.map_or_else(
            || {
                dc.max_packet_life_time.map_or(Reliability::Reliable, |lt| {
                    Reliability::MaxPacketLifetime { lifetime: lt }
                })
            },
            |r| Reliability::MaxRetransmits { retransmits: r },
        );
        let config = ChannelConfig {
            label: dc.label.clone(),
            ordered: dc.ordered,
            reliability,
            negotiated: None,
            protocol: dc.protocol.clone(),
        };
        api.add_channel_with_config(config);
    }

    // Add media transceivers (m=audio, m=video).
    //
    // `add_media` appends unconditionally, so a transceiver we have already offered has to
    // be recognised and skipped -- otherwise re-offering (publishing video after audio,
    // say) emits a second audio m-line for a track that already has one, and orphans the
    // first. The identity is the *track*, not the kind; see `offered_tracks`.
    for tc in transceivers {
        if let Some(track) = &tc.track_id
            && pc.offered_tracks.contains(&(tc.kind, track.clone()))
        {
            tracing::debug!(pc_id = %pc.id, kind = ?tc.kind, %track, "Transceiver already offered, not re-adding");
            continue;
        }
        let mid = api.add_media(tc.kind, tc.direction, None, tc.track_id.clone(), None);
        tracing::info!(
            pc_id = %pc.id,
            %mid,
            kind = ?tc.kind,
            direction = ?tc.direction,
            track_id = tc.track_id.as_deref().unwrap_or("<generated>"),
            "Added transceiver"
        );
        if let Some(track) = &tc.track_id {
            pc.offered_tracks.push((tc.kind, track.clone()));
        }
        // The first of each kind is the one we send on, and it keeps that role for the
        // life of the connection. Later sections of the same kind are the receive
        // sections protocol 17 asks for; writing to one would send our microphone down a
        // slot the SFU has allocated to somebody else.
        match tc.kind {
            MediaKind::Audio => pc.audio_mid = pc.audio_mid.or(Some(mid)),
            MediaKind::Video => pc.video_mid = pc.video_mid.or(Some(mid)),
        }
        // Recorded per track as well as per kind, because a second video m-line is a
        // second track of ours rather than somebody else's receive slot once the caller
        // has named it. `or_insert` for the same reason the fields above use `or`: the
        // first m-line offered for a track keeps the role across re-offers.
        if let Some(key) = tc.key {
            pc.send_mids.entry(key).or_insert(mid);
        }
    }

    // Nothing to change is not a failure.
    //
    // str0m returns `None` here when the requested changes leave the session as it already
    // is -- a re-offer whose transceivers and channels all exist. `createOffer` in the DOM
    // answers that with an offer describing the current state, and it must: the caller is
    // renegotiating and expects an SDP.
    //
    // Returning an error instead cost three reconnections in seventy seconds of one call.
    // It crossed the IPC as a rejected promise, livekit-client read the negotiation as
    // failed and tore the session down, and the rebuild lost the encoder's rate control,
    // the far end's decode state and, until it was fixed separately, the capture pipelines'
    // attachment. Every symptom of that call traced back to this line.
    let Some((offer, pending)) = api.apply() else {
        let Some(previous) = pc.last_offer_sdp.clone() else {
            // No changes and nothing offered before means there is genuinely nothing to
            // describe, which is a real fault rather than a quiet re-offer.
            return Err(crate::error::WebRtcError::from("No changes to apply"));
        };
        tracing::info!(
            pc_id = %pc.id,
            sdp_len = previous.len(),
            "re-offer changes nothing; returning the offer already in force"
        );
        return Ok(SessionDescription {
            sdp_type: SdpType::Offer,
            sdp: previous,
        });
    };
    pc.pending_offer = Some(pending);
    // NOTE: apply() does NOT call init_dtls — only accept_offer/accept_answer do.
    // dtls_initialized stays false until we receive the remote answer.

    let sdp = offer.to_sdp_string();
    tracing::info!(pc_id = %pc.id, sdp_len = sdp.len(), num_dc = data_channels.len(), num_tc = transceivers.len(), "Created SDP offer");
    pc.last_offer_sdp = Some(sdp.clone());

    Ok(SessionDescription {
        sdp_type: SdpType::Offer,
        sdp,
    })
}

/// Create an SDP answer (after receiving a remote offer).
/// Returns the answer that was generated during `set_remote_description(Offer)`.
///
/// # Errors
///
/// Returns an error if no answer has been cached, i.e. `set_remote_description`
/// was not called with an offer first.
pub fn create_answer(
    pc: &mut PeerConnectionInner,
) -> Result<SessionDescription, crate::error::WebRtcError> {
    pc.cached_answer
        .take()
        .ok_or_else(|| "No cached answer — call set_remote_description(offer) first".into())
}

/// Set the remote description (offer or answer).
///
/// # Errors
///
/// Returns an error if the SDP is invalid, or if str0m fails to accept the
/// offer/answer (e.g. no pending offer to match an answer against).
pub fn set_remote_description(
    pc: &mut PeerConnectionInner,
    desc: &SessionDescription,
) -> Result<Option<SessionDescription>, crate::error::WebRtcError> {
    match desc.sdp_type {
        SdpType::Offer => {
            // Log the setup direction from the remote offer
            if let Some(line) = desc.sdp.lines().find(|l| l.starts_with("a=setup:")) {
                tracing::info!(pc_id = %pc.id, setup = line, "Remote offer DTLS setup direction");
            }
            let offer = SdpOffer::from_sdp_string(&desc.sdp)
                .map_err(|e| format!("Invalid offer SDP: {e}"))?;
            let answer = pc
                .rtc
                .sdp_api()
                .accept_offer(offer)
                .map_err(|e| format!("Failed to accept offer: {e}"))?;
            // accept_offer triggers init_dtls inside str0m
            pc.dtls_initialized = true;
            tracing::info!(pc_id = %pc.id, "accept_offer completed, dtls_initialized=true");

            // Re-add cached remote ICE candidates after accept_offer.
            // str0m's accept_offer may clear remote candidates during renegotiation,
            // but livekit-client won't re-send them (deduplication).
            if !pc.remote_candidates.is_empty() {
                tracing::info!(
                    pc_id = %pc.id,
                    count = pc.remote_candidates.len(),
                    "Re-adding cached remote ICE candidates after renegotiation"
                );
                for cand_sdp in pc.remote_candidates.clone() {
                    match Candidate::from_sdp_string(&cand_sdp) {
                        Ok(candidate) => {
                            pc.rtc.add_remote_candidate(candidate);
                        }
                        Err(e) => {
                            tracing::warn!(pc_id = %pc.id, err = %e, "Failed to re-add cached candidate");
                        }
                    }
                }
            }

            let answer_sdp = answer.to_sdp_string();
            let answer_desc = SessionDescription {
                sdp_type: SdpType::Answer,
                sdp: answer_sdp,
            };
            // Cache for createAnswer() — standard WebRTC API calls createAnswer()
            // after setRemoteDescription(offer), but str0m generates it during accept_offer.
            pc.cached_answer = Some(answer_desc.clone());
            Ok(Some(answer_desc))
        }
        SdpType::Answer => {
            // Log the setup direction from the remote answer — determines DTLS initiator
            if let Some(line) = desc.sdp.lines().find(|l| l.starts_with("a=setup:")) {
                tracing::info!(pc_id = %pc.id, setup = line, "Remote answer DTLS setup direction");
            }
            let answer = SdpAnswer::from_sdp_string(&desc.sdp)
                .map_err(|e| format!("Invalid answer SDP: {e}"))?;
            let pending = pc
                .pending_offer
                .take()
                .ok_or("No pending offer to match answer")?;
            pc.rtc
                .sdp_api()
                .accept_answer(pending, answer)
                .map_err(|e| format!("Failed to accept answer: {e}"))?;
            // accept_answer triggers init_dtls inside str0m
            pc.dtls_initialized = true;
            tracing::info!(pc_id = %pc.id, "accept_answer completed, dtls_initialized=true");
            Ok(None)
        }
    }
}

/// Add a remote ICE candidate.
///
/// # Errors
///
/// Returns an error if `candidate_sdp` is not a valid ICE candidate SDP string.
pub fn add_ice_candidate(
    pc: &mut PeerConnectionInner,
    candidate_sdp: &str,
) -> Result<(), crate::error::WebRtcError> {
    if candidate_sdp.is_empty() {
        return Ok(());
    }
    tracing::info!(pc_id = %pc.id, candidate = %candidate_sdp, "Adding remote ICE candidate");
    let candidate = Candidate::from_sdp_string(candidate_sdp)
        .map_err(|e| format!("Invalid ICE candidate: {e}"))?;
    tracing::info!(pc_id = %pc.id, ?candidate, "Parsed remote ICE candidate");
    pc.rtc.add_remote_candidate(candidate);
    // Cache for re-adding after SDP renegotiation (accept_offer may clear candidates)
    if !pc.remote_candidates.contains(&candidate_sdp.to_string()) {
        pc.remote_candidates.push(candidate_sdp.to_string());
    }
    Ok(())
}

/// The wall-clock instant that corresponds to a given audio RTP offset.
///
/// str0m documents `wallclock` as "the real world time that corresponds to the
/// `MediaTime`", and uses it for exactly one thing: building the NTP<->RTP mapping in RTCP
/// Sender Reports (`streams/send.rs::sender_info`). Receivers use that mapping to drive
/// playout timing.
///
/// Passing `Instant::now()` at write time -- as this did -- makes that mapping reflect
/// *when we got round to draining the frame*, not when its samples were captured. The
/// draining loop blocks for up to 20ms waiting on the socket, so the reported mapping
/// wobbled by that much every reporting interval while the RTP timestamps advanced
/// perfectly evenly. A receiver tracking that mapping sees the sender's clock jitter and
/// keeps re-adjusting its playout, which is audible as robotic, warbling speech -- and is
/// invisible to packet-loss diagnostics, because nothing is lost.
///
/// Deriving it from the media timeline instead keeps wallclock and `MediaTime` exactly
/// consistent, which is what str0m asks for.
///
/// Clamped to never exceed `now`: if the capture thread ever runs ahead of real time, a
/// wallclock in the future makes str0m's `current_rtp_time` return `None` and emit a
/// Sender Report with `rtp_time` of zero, which is worse than a slightly stale mapping.
fn audio_wallclock(epoch: Instant, rtp_offset: u64, now: Instant) -> Instant {
    // 48kHz: RTP offset in samples -> elapsed media time.
    let elapsed = Duration::from_nanos(
        rtp_offset
            .saturating_mul(1_000_000_000)
            .checked_div(48_000)
            .unwrap_or(0),
    );
    let derived = epoch.checked_add(elapsed).unwrap_or(now);
    derived.min(now)
}

/// Write an Opus-encoded audio frame to the peer connection.
///
/// # Errors
///
/// Returns an error if no audio mid/writer is configured, if no Opus payload
/// type was negotiated, if the audio frame counter overflows, or if str0m
/// fails to write the RTP packet.
///
/// # Panics
///
/// Never panics: `48_000` is a non-zero literal, so the internal `NonZeroU32`
/// construction always succeeds.
pub fn write_audio(
    pc: &mut PeerConnectionInner,
    key: MediaTrackKey,
    opus_data: &WireMedia,
) -> Result<(), crate::error::WebRtcError> {
    let opus_data = opus_data.as_bytes();
    let mid = send_mid_for(pc, key)?;

    let Some(writer) = pc.rtc.writer(mid) else {
        return Err(crate::error::WebRtcError::NoWriterForKind("audio"));
    };

    let pt = writer
        .payload_params()
        .find(|p| p.spec().codec == Codec::Opus)
        .map(str0m::format::PayloadParams::pt)
        .ok_or("No Opus payload type negotiated")?;
    pc.audio_pt = Some(*pt);

    // Opus at 48kHz: each 20ms frame = 960 samples
    let samples_per_frame: u64 = 960;
    let rtp_offset = pc
        .audio_frame_count
        .checked_mul(samples_per_frame)
        .ok_or("audio frame offset overflow")?;
    // 48_000 is a non-zero literal, so this NonZeroU32 construction is infallible.
    #[allow(clippy::unwrap_used)]
    let rtp_time = MediaTime::new(rtp_offset, NonZeroU32::new(48_000).unwrap().into());

    let now = Instant::now();
    let epoch = *pc.audio_epoch.get_or_insert(now);
    let wallclock = audio_wallclock(epoch, rtp_offset, now);

    writer
        .write(pt, wallclock, rtp_time, opus_data)
        .map_err(|e| format!("Failed to write audio: {e}"))?;

    pc.audio_frame_count = pc
        .audio_frame_count
        .checked_add(1)
        .ok_or("audio frame counter overflow")?;
    Ok(())
}

/// The str0m codec matching one of ours.
const fn str0m_codec(codec: elementium_codec::VideoCodec) -> Codec {
    match codec {
        elementium_codec::VideoCodec::Vp8 => Codec::Vp8,
        elementium_codec::VideoCodec::H264 => Codec::H264,
        elementium_codec::VideoCodec::Av1 => Codec::Av1,
    }
}

/// Log the first bytes of a video frame at the packetiser boundary, when asked.
///
/// Off unless `ELEMENTIUM_FRAME_DUMP` is set, because it runs on every frame and the
/// question it answers is asked rarely. It exists for one comparison that nothing else can
/// make: our encrypted H.264 is forwarded correctly by the SFU and decoded by our own
/// client, and rejected by Chrome's depacketiser. A wire capture cannot settle it -- SRTP
/// hides the payload -- and the SFU's stats only say the stream parsed. What is left is
/// what each sender hands to its packetiser, and this is that byte string.
///
/// Sixteen bytes: an H.264 NAL header is one byte, an FU indicator two, a start code four,
/// and livekit's clear-header prefix for a slice is 27 -- so sixteen shows which convention
/// is in use without dumping ciphertext into a log.
fn dump_frame_head(direction: &str, codec: &str, len: usize, bytes: &[u8]) {
    if std::env::var_os("ELEMENTIUM_FRAME_DUMP").is_none() {
        return;
    }
    let head: String = bytes
        .iter()
        .take(16)
        .fold(String::new(), |mut acc, b| {
            use std::fmt::Write as _;
            let _ = write!(&mut acc, "{b:02x}");
            acc
        });
    tracing::info!(direction, codec, len, head, "frame at the packetiser boundary");
}


/// Write an encoded video frame to the peer connection.
///
/// # Errors
///
/// Returns an error if no video mid/writer is configured, if no VP8 payload
/// type was negotiated, if the video frame counter overflows, or if str0m
/// fails to write the RTP packet.
///
/// # Panics
///
/// Never panics: `90_000` is a non-zero literal, so the internal `NonZeroU32`
/// construction always succeeds.
pub fn write_video(
    pc: &mut PeerConnectionInner,
    key: MediaTrackKey,
    frame: &WireMedia,
    codec: elementium_codec::VideoCodec,
) -> Result<(), crate::error::WebRtcError> {
    let frame_data = frame.as_bytes();
    dump_frame_head("outbound", codec.sdp_name(), frame_data.len(), frame_data);
    let mid = send_mid_for(pc, key)?;

    let Some(writer) = pc.rtc.writer(mid) else {
        return Err(crate::error::WebRtcError::NoWriterForKind("video"));
    };

    // The payload type follows the codec of the bytes in hand rather than an assumption
    // about which codec that is. Writing VP8's payload type on an H.264 frame produces a
    // stream the receiver decodes as VP8 and cannot make sense of, with nothing on either
    // side reporting an error.
    let wanted = str0m_codec(codec);
    // Not simply the first payload type of the right codec.
    //
    // str0m offers seven H.264 payload types, alternating `packetization-mode` 1 and 0, and
    // its packetiser emits FU-A for any NAL larger than the MTU -- which every keyframe is.
    // A receiver that negotiated mode 0 is entitled to drop every fragment, and does: the
    // packets arrive and are counted, no frame is ever assembled, and the receiver asks for
    // a keyframe forever. Nothing on the sending side notices, because all it did was hand
    // bytes to a writer that accepted them.
    //
    // So prefer a payload type that permits fragmentation, and fall back only if the far
    // end negotiated nothing else.
    let params: Vec<_> = writer
        .payload_params()
        .filter(|p| p.spec().codec == wanted)
        .collect();
    let fragmentable = params
        .iter()
        .find(|p| p.spec().format.packetization_mode != Some(0));
    let chosen = fragmentable
        .or_else(|| params.first())
        .ok_or_else(|| format!("No {} payload type negotiated", codec.sdp_name()))?;
    if fragmentable.is_none() {
        tracing::warn!(
            pc_id = %pc.id,
            pt = ?chosen.pt(),
            "the only negotiated H.264 payload type forbids fragmentation; frames larger \
             than the MTU will not be reassembled by the receiver"
        );
    }
    let pt = chosen.pt();

    // The RTP timestamp must describe when the frame was *captured*, on the 90kHz clock.
    //
    // This used to be `frame_count * 3000`, i.e. an assumption of exactly 30fps. The
    // camera runs at 60, so the timestamps advanced at half the rate the frames actually
    // arrived: the receiver is told a second of video every two seconds it receives. Its
    // jitter buffer and render clock both work from that number, so it either stalls
    // waiting for a future it has already been sent, or displays frames at the wrong time
    // relative to audio. Nothing on our side notices, because we are simply counting.
    //
    // Measuring elapsed time instead makes the timestamps true at any capture rate, and
    // at a variable one -- which a webcam under changing light very much is.
    let now = Instant::now();
    let epoch = *pc.video_epoch.get_or_insert(now);
    let elapsed = now.saturating_duration_since(epoch);
    let rtp_offset = u64::from(elapsed.subsec_nanos())
        .saturating_mul(90_000)
        .checked_div(1_000_000_000)
        .unwrap_or(0)
        .saturating_add(elapsed.as_secs().saturating_mul(90_000));
    // 90_000 is a non-zero literal, so this NonZeroU32 construction is infallible.
    #[allow(clippy::unwrap_used)]
    let rtp_time = MediaTime::new(rtp_offset, NonZeroU32::new(90_000).unwrap().into());

    writer
        .write(pt, now, rtp_time, frame_data)
        .map_err(|e| format!("Failed to write video: {e}"))?;

    pc.video_frame_count = pc
        .video_frame_count
        .checked_add(1)
        .ok_or("video frame counter overflow")?;
    Ok(())
}

/// Write one message to a negotiated data channel, identified by the label it was created
/// with.
///
/// Returns `Ok(true)`/`Ok(false)` exactly as str0m's own `Channel::write` does: `false`
/// means the message was rejected because it would not fit in the SCTP send buffer right
/// now (not an error -- the caller can retry once `bufferedAmountLowThreshold` semantics
/// say there is room), while `Ok(true)` means str0m accepted the whole buffer.
///
/// # Errors
///
/// Returns an error if no channel named `label` has completed its DCEP handshake yet (see
/// [`PeerConnectionInner::data_channels`]), if it has since closed, or if str0m's SCTP
/// layer rejects the write outright.
pub fn write_data_channel(
    pc: &mut PeerConnectionInner,
    label: &str,
    binary: bool,
    data: &[u8],
) -> Result<bool, crate::error::WebRtcError> {
    let id = *pc
        .data_channels
        .get(label)
        .ok_or_else(|| format!("Data channel '{label}' has not completed its handshake yet"))?;
    let mut channel = pc.rtc.channel(id).ok_or_else(|| {
        format!("Data channel '{label}' has no writable SCTP stream (closed or not yet open)")
    })?;
    channel
        .write(binary, data)
        .map_err(|e| format!("Failed to write to data channel '{label}': {e}").into())
}

/// Take every data-channel event queued since the last call, for the Tauri command layer
/// to forward to the page. See [`DataChannelEvent`].
#[must_use]
pub fn drain_data_channel_events(pc: &mut PeerConnectionInner) -> Vec<DataChannelEvent> {
    std::mem::take(&mut pc.data_channel_events)
}

/// Run one iteration of the I/O loop. Returns events and next deadline.
///
/// # Errors
///
/// Returns an error if str0m fails to process a timeout, or if `poll_output`
/// reports a fatal str0m error (in which case `pc.alive` is also set to false).
pub fn poll_once(
    pc: &mut PeerConnectionInner,
    socket: &UdpSocket,
    _recv_buf: &mut [u8],
) -> Result<(Vec<WirePcEvent>, Instant), crate::error::WebRtcError> {
    // str0m's do_poll_output calls dtls.poll_output() which requires dimpl's
    // handle_timeout to have been called. But str0m's do_handle_timeout does NOT
    // propagate to dtls.handle_timeout — only init_dtls does (called during SDP
    // negotiation). So we must not call poll_output until SDP has been exchanged.
    if !pc.dtls_initialized {
        tracing::trace!(pc_id = %pc.id, "poll_once: DTLS not initialized, skipping");
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(50))
            .ok_or("deadline computation overflow")?;
        return Ok((Vec::new(), deadline));
    }

    let mut events = Vec::new();

    loop {
        // dimpl (str0m's DTLS layer) requires handle_timeout before EVERY
        // poll_output call, not just once per cycle. Feed a timeout each iteration.
        pc.rtc
            .handle_input(Input::Timeout(Instant::now()))
            .map_err(|e| format!("timeout error: {e}"))?;

        match pc.rtc.poll_output() {
            Ok(Output::Transmit(transmit)) => {
                pc.transmit_log_count = pc
                    .transmit_log_count
                    .checked_add(1)
                    .ok_or("transmit log counter overflow")?;
                let pkt_type = classify_packet(&transmit.contents);
                // Throttled for *all* packet types, media included. Emitting a formatted
                // log line per packet costs time inside the send loop, on the same thread
                // that decides when audio leaves the socket -- exactly the timing this
                // function is now instrumented to measure. Per-packet media logging would
                // perturb the measurement it exists to support.
                if pc.transmit_log_count <= 20 || pc.transmit_log_count.is_multiple_of(500) {
                    tracing::info!(
                        pc_id = %pc.id,
                        dest = %transmit.destination,
                        len = transmit.contents.len(),
                        count = pc.transmit_log_count,
                        pkt_type,
                        "Transmit → sending UDP"
                    );
                }
                let send_result = socket.send_to(&transmit.contents, transmit.destination);
                // Timestamped after the syscall returns, so the measured gap is between
                // packets actually handed to the kernel.
                record_audio_send(pc, &transmit.contents, Instant::now());
                match send_result {
                    Ok(n) => {
                        if pc.transmit_log_count <= 10 {
                            tracing::info!(pc_id = %pc.id, bytes = n, "UDP sent");
                        }
                    }
                    Err(e) => tracing::warn!(pc_id = %pc.id, err = %e, "UDP send failed"),
                }
            }
            Ok(Output::Event(event)) => {
                tracing::info!(pc_id = %pc.id, ?event, "str0m event");
                if let Some(announcement) = announce_track_on_first_media(pc, &event) {
                    events.push(announcement);
                }
                if let Some(pc_event) = handle_str0m_event(pc, event) {
                    events.push(pc_event);
                }
            }
            Ok(Output::Timeout(deadline)) => {
                return Ok((events, deadline));
            }
            Err(e) => {
                pc.alive = false;
                return Err(format!("str0m error: {e}").into());
            }
        }
    }
}

/// Try to receive a UDP packet and feed it to str0m.
///
/// # Errors
///
/// Returns an error if setting the socket read timeout fails, if the receive
/// log counter overflows, if the received length exceeds `recv_buf`, or if
/// str0m fails to process the input.
/// Receive and feed a single UDP datagram into str0m, or wait up to `timeout` for one.
///
/// Returns `Ok(true)` if a datagram was actually received (the caller should immediately
/// try again with a near-zero timeout to drain any further backlog before going back to
/// `poll_once` -- audio, video, RTCP, and STUN all share one socket, and a single video
/// keyframe/motion burst can queue several datagrams in the kernel receive buffer within
/// one 20ms audio period; only pulling one per caller iteration lets that backlog build up
/// behind bursty video, and once the kernel buffer fills the OS drops silently before
/// str0m ever sees the packet). Returns `Ok(false)` on a genuine timeout (no data was
/// waiting).
pub fn recv_and_feed(
    pc: &mut PeerConnectionInner,
    socket: &UdpSocket,
    recv_buf: &mut [u8],
    timeout: Duration,
) -> Result<bool, crate::error::WebRtcError> {
    socket
        .set_read_timeout(Some(timeout.max(Duration::from_millis(1))))
        .map_err(|e| e.to_string())?;

    match socket.recv_from(recv_buf) {
        Ok((len, source)) => {
            pc.recv_log_count = pc
                .recv_log_count
                .checked_add(1)
                .ok_or("recv log counter overflow")?;
            let recv_slice = recv_buf
                .get(..len)
                .ok_or("received length exceeds recv buffer")?;
            let pkt_type = classify_packet(recv_slice);
            if pc.recv_log_count <= 20
                || pc.recv_log_count.is_multiple_of(100)
                || pkt_type != "STUN"
            {
                tracing::info!(pc_id = %pc.id, %source, len, count = pc.recv_log_count, pkt_type, "UDP received");
            }
            // Resolve 0.0.0.0 → actual interface IP so str0m can match
            // the destination to our registered ICE candidate and generate
            // STUN Binding Responses.
            let mut dest = socket.local_addr().map_err(|e| e.to_string())?;
            if dest.ip().is_unspecified()
                && let Some(real_ip) = get_local_ip()
            {
                dest.set_ip(real_ip);
            }

            let contents_slice = recv_buf
                .get(..len)
                .ok_or("received length exceeds recv buffer")?;
            let input = Input::Receive(
                Instant::now(),
                Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: dest,
                    contents: contents_slice.try_into().map_err(|e| format!("{e:?}"))?,
                },
            );
            pc.rtc
                .handle_input(input)
                .map_err(|e| format!("handle_input error: {e}"))?;
            return Ok(true);
        }
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
        {
            pc.rtc
                .handle_input(Input::Timeout(Instant::now()))
                .map_err(|e| format!("timeout error: {e}"))?;
        }
        Err(e) => {
            return Err(format!("recv error: {e}").into());
        }
    }
    Ok(false)
}

/// Drain any further backlogged UDP datagrams beyond the one `recv_and_feed` already
/// consumed, instead of going back to `poll_once` and waiting up to the caller's normal
/// deadline again.
///
/// Audio, video, RTCP, and STUN all share one socket per connection; a burst on one (e.g.
/// a video keyframe) can queue several datagrams in the kernel receive buffer within a
/// single audio frame period. Only pulling one datagram per caller loop iteration lets
/// that backlog build up behind the burst, and once the kernel buffer fills, the OS drops
/// silently before str0m ever sees the packet. Bounded to 64 so a pathological flood can't
/// starve the rest of the caller's loop (command draining, policy refresh) indefinitely.
/// Logs (at debug) how many were drained beyond the first, when more than one -- direct
/// observability into how often/how bad this bursting actually is in practice.
pub fn drain_backlog(pc: &mut PeerConnectionInner, socket: &UdpSocket, recv_buf: &mut [u8]) {
    let mut drained = 1u32;
    while drained < 64 {
        match recv_and_feed(pc, socket, recv_buf, Duration::from_millis(1)) {
            Ok(true) => drained = drained.saturating_add(1),
            Ok(false) => break,
            Err(e) => {
                tracing::debug!(pc_id = %pc.id, error = %e, "recv_and_feed (drain) failed");
                break;
            }
        }
    }
    if drained > 1 {
        tracing::debug!(
            pc_id = %pc.id,
            drained,
            "Drained backlogged UDP datagrams in one io loop iteration"
        );
    }
}

/// Classify a UDP packet by its first byte (RFC 7983 demultiplexing).
/// Extract the primary (most recent) encoding from an RFC 2198 "RED" redundant-audio
/// packet, discarding the redundant (older) copies.
///
/// Our own SDP offers negotiate `a=rtpmap:63 red/48000/2` / `a=fmtp:63 111/111` (RED
/// wrapping Opus) for loss resilience -- common browser behavior when redundancy is
/// negotiated. str0m's `Codec::from(&str)` (str0m 0.16.2, `format/codec.rs`) has no `"red"`
/// case, so any packet that actually arrives wrapped in RED is classified `Codec::Unknown`
/// and, before this, was silently dropped -- not corrupted, just discarded, indistinguish-
/// able from ordinary loss/gaps from the receiving end's perspective. If the remote side
/// sends any meaningful fraction of its audio as RED, this alone could account for
/// significant unexplained audio gaps.
///
/// RFC 2198 format: zero or more 4-byte redundant block headers (1 bit "more follows" flag,
/// 7-bit block PT, 14-bit timestamp offset, 10-bit block length), terminated by one 1-byte
/// primary block header (flag bit clear, 7-bit primary PT, implicitly extends to the end of
/// the packet), followed by the block payloads themselves in the same order as their
/// headers. Returns `None` if the packet is too short to be valid RED framing.
fn unwrap_red_primary(payload: &[u8]) -> Option<&[u8]> {
    let mut pos = 0usize;
    let mut redundant_len_sum = 0usize;

    loop {
        let header = *payload.get(pos)?;
        let more_follows = header & 0x80 != 0;
        if !more_follows {
            // 1-byte primary header; primary payload is everything after all headers and
            // all redundant block payloads.
            let headers_len = pos.checked_add(1)?;
            let payload_start = headers_len.checked_add(redundant_len_sum)?;
            return payload.get(payload_start..);
        }
        // 4-byte redundant block header. Byte layout: byte0 = F(1 bit) + block PT(7
        // bits); byte1 = timestamp-offset bits 13..6; byte2 = timestamp-offset bits 5..0
        // (top 6 bits, unused here) + block-length bits 9..8 (bottom 2 bits); byte3 =
        // block-length bits 7..0. We only need the 10-bit block length to know how many
        // payload bytes to skip.
        let byte2 = *payload.get(pos.checked_add(2)?)?;
        let byte3 = *payload.get(pos.checked_add(3)?)?;
        let block_len = (usize::from(byte2 & 0x03) << 8) | usize::from(byte3);
        redundant_len_sum = redundant_len_sum.checked_add(block_len)?;
        pos = pos.checked_add(4)?;
    }
}

/// Payload type of an outgoing RTP packet, or `None` if the datagram is not RTP.
///
/// SRTP leaves the RTP header in the clear, so the payload type is readable even on an
/// encrypted packet. RTCP is demultiplexed off the same port (RFC 5761), which reserves
/// payload types 64..=95 for it -- those are rejected here so RTCP reports are not counted
/// as media.
fn rtp_payload_type(data: &[u8]) -> Option<u8> {
    // Shortest possible RTP packet is a 12-byte header with no payload.
    if data.len() < 12 {
        return None;
    }
    // Version must be 2 (top two bits of byte 0).
    if data.first()? & 0xC0 != 0x80 {
        return None;
    }
    // Byte 1 is [marker:1][payload type:7].
    let pt = data.get(1)? & 0x7F;
    if (64..=95).contains(&pt) {
        return None;
    }
    Some(pt)
}

/// One window's worth of measured audio send cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioSendReport {
    pub packets: u64,
    /// Sends less than 5ms after the previous one -- two frames leaving back to back.
    pub clumped: u64,
    /// Sends more than 30ms after the previous one -- a frame held back past its slot.
    pub late: u64,
    pub min_gap_ms: u64,
    pub max_gap_ms: u64,
    pub mean_gap_ms: u64,
}

/// Cadence of audio RTP packets measured at the socket, not at the point we hand them to
/// str0m.
///
/// Opus frames are produced every 20ms and must arrive at the far end spaced the same way;
/// a receiver's jitter buffer treats a late packet as a loss and conceals it, which sounds
/// robotic while every loss counter stays at zero. Existing instrumentation measured the
/// cadence of `write_audio` *calls*, which is upstream of the I/O loop's blocking socket
/// wait and so cannot see clumping introduced by it. This measures the last point we
/// control: immediately before `send_to`.
///
/// Healthy is a tight cluster at 20ms. Clumping shows up as pairs of `clumped`+`late`.
#[derive(Debug, Default)]
pub struct AudioSendPacing {
    last_send: Option<Instant>,
    packets: u64,
    clumped: u64,
    late: u64,
    min_gap_us: u64,
    max_gap_us: u64,
    total_gap_us: u64,
    gaps: u64,
}

impl AudioSendPacing {
    /// Report every 250 packets: 5 seconds of audio at 20ms per frame.
    const REPORT_EVERY: u64 = 250;
    const CLUMPED_BELOW_US: u64 = 5_000;
    const LATE_ABOVE_US: u64 = 30_000;

    /// Record one audio packet leaving the socket at `now`.
    ///
    /// Returns a report once per window, resetting the window's counters.
    pub fn record(&mut self, now: Instant) -> Option<AudioSendReport> {
        self.packets = self.packets.saturating_add(1);

        if let Some(last) = self.last_send {
            let gap_us =
                u64::try_from(now.saturating_duration_since(last).as_micros()).unwrap_or(u64::MAX);
            self.gaps = self.gaps.saturating_add(1);
            self.total_gap_us = self.total_gap_us.saturating_add(gap_us);
            self.max_gap_us = self.max_gap_us.max(gap_us);
            self.min_gap_us = if self.min_gap_us == 0 {
                gap_us
            } else {
                self.min_gap_us.min(gap_us)
            };
            if gap_us < Self::CLUMPED_BELOW_US {
                self.clumped = self.clumped.saturating_add(1);
            } else if gap_us > Self::LATE_ABOVE_US {
                self.late = self.late.saturating_add(1);
            }
        }
        self.last_send = Some(now);

        if !self.packets.is_multiple_of(Self::REPORT_EVERY) {
            return None;
        }
        let report = AudioSendReport {
            packets: self.packets,
            clumped: self.clumped,
            late: self.late,
            min_gap_ms: self.min_gap_us.checked_div(1000).unwrap_or(0),
            max_gap_ms: self.max_gap_us.checked_div(1000).unwrap_or(0),
            mean_gap_ms: self
                .total_gap_us
                .checked_div(self.gaps.max(1))
                .unwrap_or(0)
                .checked_div(1000)
                .unwrap_or(0),
        };
        // Reset the window but keep `last_send`, so the gap across the boundary is not lost.
        self.clumped = 0;
        self.late = 0;
        self.min_gap_us = 0;
        self.max_gap_us = 0;
        self.total_gap_us = 0;
        self.gaps = 0;
        Some(report)
    }
}

/// Measure the send cadence if this datagram is one of our own audio RTP packets.
fn record_audio_send(pc: &mut PeerConnectionInner, contents: &[u8], now: Instant) {
    let Some(audio_pt) = pc.audio_pt else { return };
    if rtp_payload_type(contents) != Some(audio_pt) {
        return;
    }
    if let Some(r) = pc.audio_send_pacing.record(now) {
        tracing::info!(
            pc_id = %pc.id,
            packets = r.packets,
            clumped = r.clumped,
            late = r.late,
            min_gap_ms = r.min_gap_ms,
            mean_gap_ms = r.mean_gap_ms,
            max_gap_ms = r.max_gap_ms,
            "Outbound audio socket pacing"
        );
    }
}

const fn classify_packet(data: &[u8]) -> &'static str {
    match data.first() {
        Some(0..=3) => "STUN",         // STUN Binding Request/Response/Indication
        Some(20..=63) => "DTLS", // DTLS records (ContentType: 20=ChangeCipherSpec, 21=Alert, 22=Handshake, 23=ApplicationData)
        Some(128..=191) => "RTP/RTCP", // RTP (128-191) or RTCP
        Some(b) => {
            if *b == 0 || *b == 1 {
                "STUN"
            } else {
                "UNKNOWN"
            }
        }
        None => "EMPTY",
    }
}

/// How long ICE may stay disconnected before the connection is abandoned.
///
/// Matches the order of magnitude browsers allow before declaring a peer connection failed.
/// Long enough to ride out a wifi roam or a brief route change; short enough that a
/// genuinely dead connection does not linger.
pub const ICE_DISCONNECT_GRACE: Duration = Duration::from_secs(30);

/// Whether an ICE disconnection has gone on long enough to give up on.
#[must_use]
pub fn ice_disconnect_expired(since: Option<Instant>, now: Instant) -> bool {
    since.is_some_and(|t| now.saturating_duration_since(t) >= ICE_DISCONNECT_GRACE)
}

/// Record an ICE state transition, starting or clearing the disconnection clock.
fn note_ice_state(pc: &mut PeerConnectionInner, mapped: IceState) {
    match mapped {
        IceState::Disconnected => {
            // Start the clock rather than tearing down. Recovery is normal.
            let first = pc.ice_disconnected_since.is_none();
            pc.ice_disconnected_since.get_or_insert_with(Instant::now);
            if first {
                tracing::warn!(
                    pc_id = %pc.id,
                    grace_secs = ICE_DISCONNECT_GRACE.as_secs(),
                    reason = "ice_disconnected",
                    "ICE disconnected; waiting for it to recover before giving up"
                );
            }
        }
        IceState::Connected | IceState::Completed => {
            if let Some(since) = pc.ice_disconnected_since.take() {
                tracing::info!(
                    pc_id = %pc.id,
                    down_ms = since.elapsed().as_millis(),
                    "ICE recovered after a disconnection"
                );
            }
            tracing::info!(pc_id = %pc.id, state = ?mapped, "ICE connection state changed");
        }
        IceState::New | IceState::Checking | IceState::Failed | IceState::Closed => {
            tracing::info!(pc_id = %pc.id, state = ?mapped, "ICE connection state changed");
        }
    }
}

/// Turn an inbound RTP frame into the event the rest of the stack consumes.
///
/// Split out of [`handle_str0m_event`] because it is the only arm that inspects a
/// payload, and because every codec it does not recognise has its own reason for
/// arriving -- RED, RTX and an outright unknown are three different situations that
/// happen to reach the same place.
/// Announce a remote track the first time media actually arrives on its mid.
///
/// `Event::MediaAdded` is str0m's announcement, and it fires only for m-lines the *remote*
/// side introduced. When we are the offerer -- which is every call through the Element Call
/// widget, because it adds its own receiving transceivers -- str0m has nothing to tell us:
/// we asked for those m-lines ourselves. So `MediaAdded` never fired once in a real call,
/// `RemoteTrackAdded` was never produced, the shim never built a renderer for the remote
/// video, and nothing ever read the frames.
///
/// The failure was invisible from every side that could have reported it. The far end's
/// video arrived (2350 reassembled frames in one call), was decrypted, was decoded, and was
/// written into the display buffer under a key no one asked for. The remote participant
/// was simply never shown.
///
/// Keyed on media arriving rather than on the SDP, because that is the fact that matters
/// and it holds however the m-line came to exist: a mid carrying media is a track worth
/// showing, whoever offered it.
fn announce_track_on_first_media(
    pc: &mut PeerConnectionInner,
    event: &Event,
) -> Option<WirePcEvent> {
    let Event::MediaData(data) = event else {
        return None;
    };
    let kind = if data.params.spec().codec == Codec::Opus {
        MediaKind::Audio
    } else {
        MediaKind::Video
    };
    if !is_first_media_for_mid(&mut pc.remote_mids, data.mid, kind) {
        return None;
    }
    tracing::info!(
        pc_id = %pc.id,
        mid = %data.mid,
        ?kind,
        "remote track announced from its first media, having never been announced by SDP"
    );
    Some(PcEvent::RemoteTrackAdded {
        mid: data.mid.to_string(),
        kind: match kind {
            MediaKind::Audio => "audio".to_string(),
            MediaKind::Video => "video".to_string(),
        },
    })
}

/// Record a mid as announced, reporting whether this call was the first to do so.
///
/// `remote_mids` is also what `Event::MediaAdded` fills in, so a track the SDP did announce
/// is not announced a second time when its media arrives.
fn is_first_media_for_mid(
    announced: &mut HashMap<Mid, MediaKind>,
    mid: Mid,
    kind: MediaKind,
) -> bool {
    announced.insert(mid, kind).is_none()
}

fn media_data_event(
    pc: &mut PeerConnectionInner,
    data: str0m::media::MediaData,
) -> Option<WirePcEvent> {
    let mid = data.mid.to_string();
    let contiguous = data.contiguous;
    let codec = data.params.spec().codec;
    if codec == Codec::Opus {
        // Opus frames must reach the decoder in encode order. Out-of-order
        // delivery still decodes cleanly packet-by-packet but scrambles the audio
        // in time -- inaudible to every error/amplitude check, so detect it
        // explicitly here rather than assuming str0m always emits in order.
        let first_seq: u64 = **data.seq_range.start();
        let last_seq: u64 = **data.seq_range.end();
        if let Some(prev) = pc.last_audio_seq.insert(data.mid, last_seq)
            && first_seq <= prev
        {
            tracing::warn!(
                pc_id = %pc.id,
                mid,
                prev_seq = prev,
                this_seq_start = first_seq,
                this_seq_end = last_seq,
                "Inbound audio frame arrived OUT OF ORDER: scrambles decoded audio in time"
            );
        }
        Some(PcEvent::AudioData {
            mid,
            data: WireMedia::from_network(data.data),
            contiguous,
        })
    } else if codec == Codec::Vp8 || codec == Codec::H264 {
        // H.264 arrives here as well as VP8, and the codec goes with it. Before this it
        // fell through to "Unhandled codec ... dropping": a peer publishing H.264 was a
        // black tile with a debug line nobody was reading, which is the same failure the
        // decoder was added to remove and would have survived it.
        let video = if codec == Codec::Vp8 {
            elementium_codec::VideoCodec::Vp8
        } else {
            elementium_codec::VideoCodec::H264
        };
        // The other half of the comparison: a frame str0m has just reassembled from a
        // *remote* sender. Subscribed to a browser, this is what libwebrtc's packetiser was
        // given, recovered by depacketising it -- the reference our own outbound bytes are
        // measured against.
        dump_frame_head("inbound", video.sdp_name(), data.data.len(), &data.data);
        Some(PcEvent::VideoData {
            mid,
            data: WireMedia::from_network(data.data),
            codec: video,
        })
    } else if codec == Codec::Unknown && pc.remote_mids.get(&data.mid) == Some(&MediaKind::Audio) {
        // Almost certainly RFC 2198 RED (str0m has no "red" Codec variant, so RED
        // packets land here as Unknown) -- unwrap it instead of dropping it.
        if let Some(primary) = unwrap_red_primary(&data.data) {
            Some(PcEvent::AudioData {
                mid,
                data: WireMedia::from_network(primary.to_vec()),
                contiguous,
            })
        } else {
            tracing::warn!(pc_id = %pc.id, mid, len = data.data.len(), "Unhandled audio codec on inbound media (not RED-decodable), dropping");
            None
        }
    } else if codec == Codec::Rtx {
        // Expected under any packet loss (retransmission requests); not a bug,
        // and warning on every one would drown out real signal.
        None
    } else {
        tracing::warn!(pc_id = %pc.id, mid, codec = %codec, "Unhandled codec on inbound media, dropping");
        None
    }
}

fn handle_str0m_event(pc: &mut PeerConnectionInner, event: Event) -> Option<WirePcEvent> {
    match event {
        Event::IceConnectionStateChange(state) => {
            let mapped = match state {
                IceConnectionState::New => IceState::New,
                IceConnectionState::Checking => IceState::Checking,
                IceConnectionState::Connected => IceState::Connected,
                IceConnectionState::Completed => IceState::Completed,
                IceConnectionState::Disconnected => IceState::Disconnected,
            };
            note_ice_state(pc, mapped);
            Some(PcEvent::IceConnectionStateChange(mapped))
        }
        Event::Connected => {
            tracing::info!(pc_id = %pc.id, "DTLS/ICE connection established");
            Some(PcEvent::Connected)
        }
        Event::MediaAdded(media) => {
            tracing::info!(pc_id = %pc.id, ?media, "Media added");
            pc.remote_mids.insert(media.mid, media.kind);
            match media.kind {
                MediaKind::Audio if pc.audio_mid.is_none() => {
                    pc.audio_mid = Some(media.mid);
                }
                MediaKind::Video if pc.video_mid.is_none() => {
                    pc.video_mid = Some(media.mid);
                }
                _ => {}
            }
            // Answering a remote offer: the m-lines are the offerer's, so none of them
            // carries a name of ours. The first of each kind is what we send on, under the
            // default key -- a share published this way is not possible, because the plain
            // connection has no way to announce a second video track in the first place.
            pc.send_mids
                .entry(default_key_for(media.kind))
                .or_insert(media.mid);
            Some(PcEvent::RemoteTrackAdded {
                mid: media.mid.to_string(),
                kind: match media.kind {
                    MediaKind::Audio => "audio".to_string(),
                    MediaKind::Video => "video".to_string(),
                },
            })
        }
        Event::MediaData(data) => media_data_event(pc, data),
        Event::MediaEgressStats(stats) => {
            pc.egress_stats.insert(stats.mid, stats.clone());
            Some(egress_stats_event(&pc.id, &stats))
        }
        // Not surfaced as a `PcEvent` -- nothing downstream in this crate currently
        // consumes per-track inbound RTCP the way `EgressStats` feeds the encoder's FEC
        // sizing. Recorded here so `getStats()` has real inbound numbers to report;
        // storing it is the whole fix, no event needs to travel further.
        Event::MediaIngressStats(stats) => {
            pc.ingress_stats.insert(stats.mid, stats);
            None
        }
        Event::PeerStats(stats) => {
            pc.peer_stats = Some(stats);
            None
        }
        Event::KeyframeRequest(req) => {
            tracing::info!(
                pc_id = %pc.id,
                mid = %req.mid,
                kind = ?req.kind,
                "Receiver requested a keyframe"
            );
            Some(PcEvent::KeyframeRequested {
                mid: req.mid.to_string(),
            })
        }
        // Data-channel events are queued on `pc.data_channel_events` rather than
        // returned as a `PcEvent` -- see `DataChannelEvent` for why. `None` here means
        // "handled", not "unhandled".
        Event::ChannelOpen(id, label) => {
            tracing::info!(pc_id = %pc.id, %label, "Data channel open");
            pc.data_channels.insert(label.clone(), id);
            pc.data_channel_labels.insert(id, label.clone());
            pc.data_channel_events.push(DataChannelEvent::Open { label });
            None
        }
        Event::ChannelData(data) => {
            if let Some(label) = pc.data_channel_labels.get(&data.id).cloned() {
                pc.data_channel_events.push(DataChannelEvent::Message {
                    label,
                    binary: data.binary,
                    data: data.data,
                });
            } else {
                tracing::warn!(
                    pc_id = %pc.id,
                    id = ?data.id,
                    "Data channel message for an unrecognised channel id, dropping"
                );
            }
            None
        }
        Event::ChannelClose(id) => {
            if let Some(label) = pc.data_channel_labels.remove(&id) {
                pc.data_channels.remove(&label);
                tracing::info!(pc_id = %pc.id, %label, "Data channel closed");
                pc.data_channel_events.push(DataChannelEvent::Close { label });
            }
            None
        }
        other => {
            note_diagnostic_event(pc, other);
            None
        }
    }
}

/// Events that change no state here, split by whether anyone would want to read them.
///
/// The "unhandled" bucket is only useful if what lands in it is genuinely unconsidered. One
/// call put 898 `SenderFeedback` events through it in two minutes, which is how a bucket
/// stops being read at all.
fn note_diagnostic_event(pc: &PeerConnectionInner, event: Event) {
    match event {
        // A remote stream stopped or resumed delivering. str0m raises this after roughly a
        // second and a half of silence on a receive stream, which is the closest thing we
        // get to "their picture has frozen" -- worth naming, because it arrives at exactly
        // the moment someone says the other person has gone still.
        Event::StreamPaused(paused) => {
            tracing::info!(
                pc_id = %pc.id,
                mid = %paused.mid,
                paused = paused.paused,
                "remote stream paused state changed"
            );
        }
        // The SCTP send buffer has drained. `write_data_channel` returns `Ok(false)` when
        // the buffer is full and its documentation tells callers to retry once there is
        // room; this is the event that says there is. Nothing acts on it yet, so it is
        // named rather than left looking like something nobody considered.
        Event::ChannelBufferedAmountLow(id) => {
            tracing::debug!(
                pc_id = %pc.id,
                channel = ?id,
                "data channel send buffer drained; a blocked writer could retry now"
            );
        }
        // RTCP Sender Reports for streams we *receive* -- `poll_sender_feedback` walks
        // `streams_rx`, so despite the name this is the remote describing its own sending,
        // not feedback about ours. Ours arrives as `MediaEgressStats`, handled above.
        // Consumed silently; they would matter if this ever did audio/video
        // synchronisation, which needs the NTP-to-RTP mapping they carry.
        Event::SenderFeedback(_) => {}
        other => {
            tracing::info!(pc_id = %pc.id, event = ?other, "Unhandled str0m event");
        }
    }
}

/// Convert str0m's aggregated egress stats into a [`PcEvent::EgressStats`].
///
/// `loss` is `None` when no RTCP receiver report arrived during the interval, which is
/// normal on a track we are not currently sending on. That is deliberately *not* the same
/// as "zero loss" and is passed through as `None`: flattening it to `0.0` would decay the
/// downstream loss estimate toward "clean link" simply because reports stopped arriving.
fn egress_stats_event(pc_id: &str, stats: &str0m::stats::MediaEgressStats) -> WirePcEvent {
    tracing::debug!(
        pc_id,
        mid = %stats.mid,
        loss = ?stats.loss,
        rtt_ms = ?stats.rtt.map(|d| d.as_millis()),
        packets = stats.packets,
        nacks = stats.nacks,
        "Outbound RTCP stats"
    );
    PcEvent::EgressStats {
        mid: stats.mid.to_string(),
        loss: stats.loss,
        rtt_ms: stats.rtt.and_then(|d| u64::try_from(d.as_millis()).ok()),
        packets: stats.packets,
        nacks: stats.nacks,
    }
}

/// Transport stats for one of our outbound (sent) tracks.
///
/// In plain types so callers outside this crate (the Tauri command layer, which does not
/// depend on `str0m`) can serialize them without reaching for the `str0m` types directly.
///
/// `round_trip_time_ms`, `remote_packets_lost` and `remote_jitter_rtp_units` are all
/// `None` rather than `0` when str0m has not reported them, per the rule this type exists
/// to enforce: a `0` here would be indistinguishable from a real "clean" measurement, and
/// `getStats()` callers (connection-quality indicators) treat the two very differently.
#[derive(Debug, Clone)]
pub struct OutboundTransportStats {
    pub mid: String,
    /// `None` when this mid is not one this side recognises as audio or video -- should
    /// not happen in practice (egress stats only exist for tracks we negotiated), but
    /// there is no reason to assert it here.
    pub kind: Option<&'static str>,
    pub bytes_sent: u64,
    pub packets_sent: u64,
    pub nack_count: u64,
    pub pli_count: u64,
    pub fir_count: u64,
    /// Round-trip time extracted from the peer's most recent RTCP receiver report for
    /// this track.
    pub round_trip_time_ms: Option<u64>,
    /// Cumulative packets the peer's RTCP receiver reports say it never received. This is
    /// the peer's own count, not a local recomputation -- str0m has nothing else to offer,
    /// and no local guess gets any closer to what the peer actually experienced.
    pub remote_packets_lost: Option<u64>,
    /// Jitter as measured by the peer, in raw RTP timestamp ticks (str0m does not convert
    /// this to seconds). Left as ticks rather than divided here: the correct clock rate
    /// depends on the codec (Opus 48 kHz vs VP8/H.264 90 kHz per RFC 7587 §4, RFC 6386
    /// §2, RFC 6184 §2), and `kind` above is the cheapest place to make that conversion
    /// correctly -- doing it here with the wrong assumption would be worse than not doing
    /// it, so it is left for whoever has `kind` in hand.
    pub remote_jitter_rtp_units: Option<u32>,
}

/// Transport stats for one of our inbound (received) tracks. See
/// [`OutboundTransportStats`] for why every optional field stays `None` instead of `0`
/// when unmeasured.
#[derive(Debug, Clone)]
pub struct InboundTransportStats {
    pub mid: String,
    pub kind: Option<&'static str>,
    pub bytes_received: u64,
    pub packets_received: u64,
    pub nack_count: u64,
    pub pli_count: u64,
    pub fir_count: u64,
    /// Fraction (`0.0..=1.0`) of the peer's packets we did not receive, extracted from
    /// the RTCP receiver report we ourselves last sent. Not the same shape as a spec
    /// `packetsLost` (a cumulative count): str0m only exposes the fraction for inbound
    /// media, so that is what this reports, under its own name rather than mislabeled.
    pub loss_fraction: Option<f32>,
}

/// The ICE candidate pair str0m has selected, with its round-trip time.
#[derive(Debug, Clone, Default)]
pub struct CandidatePairTransportStats {
    pub round_trip_time_ms: Option<u64>,
    pub local_addr: Option<String>,
    pub remote_addr: Option<String>,
}

/// Every transport stat this crate currently has for a peer connection.
///
/// Gathered from whatever `Event::MediaEgressStats`/`MediaIngressStats`/`PeerStats` have
/// arrived so far. Built fresh on each call from the snapshots `handle_str0m_event`
/// already maintains -- never blocks on str0m and never invents a number it does not
/// have.
#[derive(Debug, Clone, Default)]
pub struct TransportStatsSnapshot {
    pub outbound: Vec<OutboundTransportStats>,
    pub inbound: Vec<InboundTransportStats>,
    pub candidate_pair: Option<CandidatePairTransportStats>,
}

/// Which media kind a mid carries, for tagging a stats entry.
///
/// Checked against the tracks this side actually knows about rather than guessed: a
/// wrong answer here would point a video quality indicator at audio numbers or vice
/// versa, which is worse than the `None` this returns when the mid is not recognised.
fn mid_kind(pc: &PeerConnectionInner, mid: Mid) -> Option<&'static str> {
    if let Some(kind) = pc.remote_mids.get(&mid) {
        return Some(media_kind_str(*kind));
    }
    if pc.audio_mid == Some(mid) {
        return Some("audio");
    }
    if pc.video_mid == Some(mid) {
        return Some("video");
    }
    None
}

const fn media_kind_str(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Audio => "audio",
        MediaKind::Video => "video",
    }
}

/// Build the current transport-stats snapshot for `getStats()` from whatever str0m has
/// reported so far. See [`TransportStatsSnapshot`].
#[must_use]
pub fn transport_stats_snapshot(pc: &PeerConnectionInner) -> TransportStatsSnapshot {
    let outbound = pc
        .egress_stats
        .values()
        .map(|stats| {
            let kind = mid_kind(pc, stats.mid);
            OutboundTransportStats {
                mid: stats.mid.to_string(),
                kind,
                bytes_sent: stats.bytes,
                packets_sent: stats.packets,
                nack_count: stats.nacks,
                pli_count: stats.plis,
                fir_count: stats.firs,
                round_trip_time_ms: stats.rtt.and_then(|d| u64::try_from(d.as_millis()).ok()),
                remote_packets_lost: stats.remote.as_ref().map(|r| r.packets_lost),
                remote_jitter_rtp_units: stats.remote.as_ref().map(|r| r.jitter),
            }
        })
        .collect();

    let inbound = pc
        .ingress_stats
        .values()
        .map(|stats| InboundTransportStats {
            mid: stats.mid.to_string(),
            kind: mid_kind(pc, stats.mid),
            bytes_received: stats.bytes,
            packets_received: stats.packets,
            nack_count: stats.nacks,
            pli_count: stats.plis,
            fir_count: stats.firs,
            loss_fraction: stats.loss,
        })
        .collect();

    let candidate_pair = pc.peer_stats.as_ref().map(|stats| {
        let pair = stats.selected_candidate_pair.as_ref();
        CandidatePairTransportStats {
            round_trip_time_ms: stats.rtt.and_then(|d| u64::try_from(d.as_millis()).ok()),
            local_addr: pair.map(|p| p.local.addr.to_string()),
            remote_addr: pair.map(|p| p.remote.addr.to_string()),
        }
    });

    TransportStatsSnapshot {
        outbound,
        inbound,
        candidate_pair,
    }
}

/// Lock the PC handle, recovering from poisoned locks.
///
/// A poisoned lock means a previous holder panicked — we recover the
/// inner data and keep going rather than cascading the panic.
pub(crate) fn lock_pc(
    handle: &PeerConnectionHandle,
) -> std::sync::MutexGuard<'_, PeerConnectionInner> {
    match handle.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!("PC lock was poisoned, recovering");
            poisoned.into_inner()
        }
    }
}

/// Perform STUN discovery using ICE servers and add srflx candidates.
pub(crate) fn discover_and_add_srflx(
    socket: &UdpSocket,
    pc: &mut PeerConnectionInner,
    local_addr: SocketAddr,
    servers: &[crate::engine::IceServerConfig],
) {
    for server in servers {
        for url in &server.urls {
            if let Some(stun_addr) = crate::stun::parse_stun_url(url) {
                tracing::info!(
                    pc_id = %pc.id,
                    %url,
                    %stun_addr,
                    "Attempting STUN discovery"
                );
                if let Some(srflx_addr) = crate::stun::discover_srflx(socket, stun_addr) {
                    // Use the real local IP as the base (not 0.0.0.0)
                    let base = if local_addr.ip().is_unspecified() {
                        let real_ip = get_local_ip()
                            .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
                        SocketAddr::new(real_ip, local_addr.port())
                    } else {
                        local_addr
                    };
                    add_srflx_candidate(pc, srflx_addr, base);
                    return; // One srflx candidate is enough
                }
            }
        }
    }
    tracing::warn!(pc_id = %pc.id, "STUN discovery failed on all ICE servers");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every remote track has to be announced, and announced once.
    ///
    /// str0m announces only the m-lines the *remote* side introduced, and in a call driven
    /// by the Element Call widget every m-line is one we offered -- so nothing was ever
    /// announced, no renderer was ever built, and the far end's video was decoded into a
    /// buffer no one read. Announcing on first media covers that; announcing again on
    /// every subsequent packet would rebuild the renderer sixty times a second.
    #[test]
    fn a_mid_is_announced_exactly_once() {
        let mut announced = HashMap::new();
        let video = Mid::from("n89");
        let audio = Mid::from("XPq");

        assert!(is_first_media_for_mid(&mut announced, video, MediaKind::Video));
        assert!(!is_first_media_for_mid(&mut announced, video, MediaKind::Video));
        assert!(!is_first_media_for_mid(&mut announced, video, MediaKind::Video));

        // A second track on the same connection is its own announcement: on an SFU every
        // participant arrives on one connection, told apart only by mid.
        assert!(is_first_media_for_mid(&mut announced, audio, MediaKind::Audio));
        assert!(!is_first_media_for_mid(&mut announced, audio, MediaKind::Audio));
    }

    /// A track the SDP announced must not be announced a second time when media arrives.
    #[test]
    fn a_track_already_announced_by_sdp_is_not_announced_again() {
        let mut announced = HashMap::new();
        let mid = Mid::from("n89");
        // What `Event::MediaAdded` does.
        announced.insert(mid, MediaKind::Video);
        assert!(!is_first_media_for_mid(&mut announced, mid, MediaKind::Video));
    }

    /// Regression test for a real incident: a poisoned PC lock (a previous holder
    /// panicked while holding it) must not cascade the panic to every subsequent
    /// caller. `lock_pc` recovers the inner data instead, matching the actual behavior
    /// str0m needs -- a connection whose I/O loop hit an unexpected panic once should
    /// keep serving requests, not become permanently unusable.
    #[test]
    // Deliberately poisoning a lock to test recovery from it requires a real panic on
    // another thread; unwrap() on the test's own assertions is idiomatic test-fail style.
    // significant_drop_tightening false-positives on the assert! immediately after the
    // guard's last use inside its own scope.
    #[allow(
        clippy::unwrap_used,
        clippy::panic,
        clippy::significant_drop_tightening
    )]
    fn lock_pc_recovers_from_a_poisoned_lock() {
        let handle: PeerConnectionHandle = std::sync::Arc::new(std::sync::Mutex::new(
            create_peer_connection("test-pc".to_string()),
        ));

        // Poison the lock by panicking while holding it, on another thread.
        let poison_handle = handle.clone();
        let result = std::thread::spawn(move || {
            let _guard = poison_handle.lock().unwrap();
            panic!("deliberate poison for lock_pc_recovers_from_a_poisoned_lock");
        })
        .join();
        assert!(result.is_err(), "the poisoning thread should have panicked");
        assert!(handle.is_poisoned(), "the lock should now be poisoned");

        // lock_pc must recover rather than panic itself.
        {
            let guard = lock_pc(&handle);
            assert_eq!(guard.id, "test-pc");
        }
    }

    /// Pins the media/control split `engine::io_loop` routes on: exactly `AudioData` and
    /// `VideoData` are droppable media, everything else is undroppable control. This is
    /// exactly the classification the bug report was about -- a wrong answer here would
    /// silently mean either media on the undroppable channel (defeating the point of
    /// keeping it bounded) or a control event on the droppable one (reintroducing the
    /// frozen `connectionState` bug this split exists to fix). Exhaustive over every
    /// variant so a new `PcEvent` variant added later has to be placed here too, rather
    /// than defaulting to whichever side is picked without anyone noticing.
    #[test]
    fn is_media_classifies_every_pc_event_variant() {
        let media_events: Vec<PcEvent> = vec![
            PcEvent::AudioData {
                mid: "0".to_string(),
                data: PlaintextMedia::from_encoder(Vec::new()),
                contiguous: true,
            },
            PcEvent::VideoData {
                mid: "0".to_string(),
                data: PlaintextMedia::from_encoder(Vec::new()),
                codec: elementium_codec::VideoCodec::Vp8,
            },
        ];
        for event in media_events {
            assert!(event.is_media(), "{event:?} should be droppable media");
        }

        let control_events: Vec<PcEvent> = vec![
            PcEvent::IceConnectionStateChange(IceState::Connected),
            PcEvent::ConnectionStateChange(PeerConnectionState::Connected),
            PcEvent::IceCandidate("candidate:0".to_string()),
            PcEvent::IceGatheringComplete,
            PcEvent::Connected,
            PcEvent::RemoteTrackAdded {
                mid: "0".to_string(),
                kind: "audio".to_string(),
            },
            PcEvent::KeyframeRequested {
                mid: "0".to_string(),
            },
            PcEvent::EgressStats {
                mid: "0".to_string(),
                loss: Some(0.0),
                rtt_ms: Some(10),
                packets: 1,
                nacks: 0,
            },
        ];
        for event in control_events {
            assert!(
                !event.is_media(),
                "{event:?} should be undroppable control"
            );
        }
    }

    /// Regression test for the `recv_and_feed` return-value contract that `drain_backlog`
    /// (and both call sites' loop pacing) now depend on: with nothing waiting on the
    /// socket, it must return `Ok(false)` (a genuine timeout), not `Ok(true)` or hang past
    /// the requested timeout. This is the steady-state case (no backlog) that both io
    /// loops hit on every iteration when the network is quiet -- if this ever regressed to
    /// `Ok(true)`, every caller would spin `drain_backlog` needlessly on every idle tick.
    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn recv_and_feed_returns_false_on_a_genuine_timeout() {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind test socket");
        let mut pc = create_peer_connection("timeout-test-pc".to_string());
        let mut recv_buf = vec![0u8; 2000];

        let start = Instant::now();
        let received = recv_and_feed(&mut pc, &socket, &mut recv_buf, Duration::from_millis(20))
            .expect("timeout is not an error");
        let elapsed = start.elapsed();

        assert!(
            !received,
            "no datagram was sent; must report no data received"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "must not hang well past the requested timeout, took {elapsed:?}"
        );
    }

    /// `drain_backlog` must terminate promptly (not hang or spin unboundedly) when there is
    /// no further backlog beyond the packet the caller already consumed -- the common case
    /// on every loop iteration, since bursts are the exception not the rule.
    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn drain_backlog_returns_promptly_with_no_further_backlog() {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind test socket");
        let mut pc = create_peer_connection("drain-test-pc".to_string());
        let mut recv_buf = vec![0u8; 2000];

        let start = Instant::now();
        drain_backlog(&mut pc, &socket, &mut recv_buf);
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(500),
            "must not hang or spin waiting for backlog that doesn't exist, took {elapsed:?}"
        );
    }

    /// Regression test for a real, confirmed protocol-level gap: our own SDP negotiates
    /// `a=rtpmap:63 red/48000/2` (RFC 2198 RED wrapping Opus), but str0m has no `"red"`
    /// `Codec` variant -- RED packets classify as `Codec::Unknown` and, before this fix,
    /// were silently dropped rather than corrupted. This proves `unwrap_red_primary`
    /// correctly extracts the primary encoding for the two real shapes a RED packet can
    /// take: no redundancy yet (stream start) and one redundant block (steady state).
    #[test]
    fn unwrap_red_primary_extracts_the_current_encoding() {
        // Case 1: no redundant blocks yet (e.g. the very first packet of a stream). Just
        // a 1-byte primary header (F=0, PT=111) followed by the primary payload.
        let primary_only = [&[0x6F][..], &[0xAA, 0xBB, 0xCC]].concat();
        assert_eq!(
            unwrap_red_primary(&primary_only),
            Some(&[0xAA, 0xBB, 0xCC][..])
        );

        // Case 2: one redundant block (an older, already-decoded frame carried again for
        // loss resilience) followed by the primary block -- the steady-state shape once a
        // stream has been running for more than one packet.
        //   Redundant header (4 bytes): F=1, PT=111 (0x80 | 0x6F = 0xEF), timestamp
        //   offset=0 (unused by this function), block length=2 (redundant payload is 2
        //   bytes): byte1=0x00, byte2=0x00 (top 2 length bits = 0), byte3=0x02.
        //   Primary header (1 byte): F=0, PT=111 (0x6F).
        let redundant_header = [0xEF, 0x00, 0x00, 0x02];
        let redundant_payload = [0x11, 0x22]; // 2 bytes, matches block length above
        let primary_header = [0x6F];
        let primary_payload = [0xAA, 0xBB, 0xCC, 0xDD];
        let with_redundancy = [
            &redundant_header[..],
            &primary_header[..],
            &redundant_payload[..],
            &primary_payload[..],
        ]
        .concat();
        assert_eq!(
            unwrap_red_primary(&with_redundancy),
            Some(&primary_payload[..])
        );
    }

    /// Malformed/truncated RED framing must fail closed (`None`), never panic or return
    /// out-of-bounds/garbage data -- this runs on untrusted network input.
    #[test]
    fn unwrap_red_primary_rejects_truncated_input() {
        assert_eq!(unwrap_red_primary(&[]), None);
        // A redundant block header (F=1) claiming a huge block length that exceeds the
        // actual packet size.
        assert_eq!(unwrap_red_primary(&[0xEF, 0x00, 0x03, 0xFF, 0x6F]), None);
        // Truncated mid-header (only 2 of 4 redundant header bytes present).
        assert_eq!(unwrap_red_primary(&[0xEF, 0x00]), None);
    }

    /// `transport_stats_snapshot` must carry every real number str0m gave it through
    /// unchanged (bytes, packets, RTCP counters, the peer's own loss/RTT/jitter
    /// measurements) and must resolve `kind` from the tracks this side actually
    /// negotiated -- not guess it, since a wrong `kind` would point Element Call's video
    /// quality indicator at audio numbers or vice versa.
    #[test]
    // `.expect()` on the lookups below is the test failing loudly if the mapping under
    // test dropped an entry -- not a real fallibility this function needs to handle.
    #[allow(clippy::expect_used)]
    fn transport_stats_snapshot_maps_real_str0m_numbers_without_inventing_any() {
        let mut pc = create_peer_connection("stats-test-pc".to_string());
        let audio_mid = Mid::from("0");
        let video_mid = Mid::from("1");
        pc.audio_mid = Some(audio_mid);
        pc.video_mid = Some(video_mid);

        let now = Instant::now();
        pc.egress_stats.insert(
            audio_mid,
            str0m::stats::MediaEgressStats {
                mid: audio_mid,
                rid: None,
                bytes: 4_000,
                packets: 50,
                firs: 0,
                plis: 1,
                nacks: 2,
                rtt: Some(Duration::from_millis(80)),
                loss: Some(0.01),
                timestamp: now,
                remote: Some(str0m::stats::RemoteIngressStats {
                    jitter: 480, // 10ms at Opus's 48kHz clock
                    maximum_sequence_number: str0m::rtp::SeqNo::from(1000u64),
                    packets_lost: 3,
                }),
            },
        );
        // No RTCP receiver report arrived for the video track this interval -- `remote`
        // must come back `None`, not a fabricated zero.
        pc.egress_stats.insert(
            video_mid,
            str0m::stats::MediaEgressStats {
                mid: video_mid,
                rid: None,
                bytes: 90_000,
                packets: 120,
                firs: 0,
                plis: 0,
                nacks: 0,
                rtt: None,
                loss: None,
                timestamp: now,
                remote: None,
            },
        );
        pc.ingress_stats.insert(
            audio_mid,
            str0m::stats::MediaIngressStats {
                mid: audio_mid,
                rid: None,
                bytes: 2_000,
                packets: 25,
                firs: 0,
                plis: 0,
                nacks: 1,
                rtt: None,
                loss: Some(0.02),
                timestamp: now,
                remote: None,
            },
        );

        let snapshot = transport_stats_snapshot(&pc);

        let outbound_audio = snapshot
            .outbound
            .iter()
            .find(|s| s.mid == audio_mid.to_string())
            .expect("audio egress entry present");
        assert_eq!(outbound_audio.kind, Some("audio"));
        assert_eq!(outbound_audio.bytes_sent, 4_000);
        assert_eq!(outbound_audio.packets_sent, 50);
        assert_eq!(outbound_audio.nack_count, 2);
        assert_eq!(outbound_audio.pli_count, 1);
        assert_eq!(outbound_audio.round_trip_time_ms, Some(80));
        assert_eq!(outbound_audio.remote_packets_lost, Some(3));
        assert_eq!(outbound_audio.remote_jitter_rtp_units, Some(480));

        let outbound_video = snapshot
            .outbound
            .iter()
            .find(|s| s.mid == video_mid.to_string())
            .expect("video egress entry present");
        assert_eq!(outbound_video.kind, Some("video"));
        // No receiver report this interval: must be `None`, never `0`.
        assert_eq!(outbound_video.round_trip_time_ms, None);
        assert_eq!(outbound_video.remote_packets_lost, None);
        assert_eq!(outbound_video.remote_jitter_rtp_units, None);

        let inbound_audio = snapshot
            .inbound
            .first()
            .expect("audio ingress entry present");
        assert_eq!(inbound_audio.kind, Some("audio"));
        assert_eq!(inbound_audio.bytes_received, 2_000);
        assert_eq!(inbound_audio.packets_received, 25);
        assert_eq!(inbound_audio.nack_count, 1);
        assert_eq!(inbound_audio.loss_fraction, Some(0.02));

        // No `Event::PeerStats` has arrived: the candidate pair must be absent, not a
        // default-constructed stand-in with fabricated zeros.
        assert!(snapshot.candidate_pair.is_none());
    }

    /// `write_data_channel` must fail closed for a label whose DCEP handshake never
    /// completed, rather than silently accepting (and dropping) the write. This is the
    /// exact shape of bug this feature exists to fix, one layer down: a `send()` that
    /// resolves as if it worked while nothing actually left the machine. There is no
    /// `str0m::channel::ChannelId` constructible outside `str0m` itself (its only field is
    /// private to that crate), so this cannot go further and assert a real handshake
    /// without a live peer -- that part is exercised by manual/E2E testing instead.
    #[test]
    fn write_data_channel_rejects_an_unopened_label() {
        let mut pc = create_peer_connection("dc-test-pc".to_string());
        let result = write_data_channel(&mut pc, "matrix", false, b"hello");
        assert!(
            result.is_err(),
            "writing to a label with no completed DCEP handshake must error, not silently \
             succeed"
        );
    }

    /// `drain_data_channel_events` must hand back everything queued and leave the queue
    /// empty, so the Tauri forwarding loop's next poll does not re-deliver the same
    /// message twice.
    #[test]
    fn drain_data_channel_events_takes_and_clears_the_queue() {
        let mut pc = create_peer_connection("dc-drain-test-pc".to_string());
        pc.data_channel_events.push(DataChannelEvent::Open {
            label: "matrix".to_string(),
        });
        pc.data_channel_events.push(DataChannelEvent::Message {
            label: "matrix".to_string(),
            binary: false,
            data: b"hi".to_vec(),
        });

        let drained = drain_data_channel_events(&mut pc);
        assert_eq!(drained.len(), 2);
        assert!(
            pc.data_channel_events.is_empty(),
            "the queue must be empty after draining, or the next poll re-delivers stale events"
        );

        // A second drain with nothing new queued must come back empty, not repeat the
        // first drain's contents.
        assert!(drain_data_channel_events(&mut pc).is_empty());
    }
}

#[cfg(test)]
mod audio_wallclock_tests {
    use super::*;

    /// The whole point: wallclock must track the media timeline, so that a receiver's
    /// NTP<->RTP mapping is stable even when our draining loop is late.
    #[test]
    fn wallclock_follows_the_media_timeline_not_the_drain_time() {
        let epoch = Instant::now();
        // 50 frames of 20ms = 1s of media.
        let one_second_of_media = 50 * 960;
        // Pretend the drain happened 17ms late; the answer must not move.
        let late_now = epoch + Duration::from_millis(1017);
        let derived = audio_wallclock(epoch, one_second_of_media, late_now);
        assert_eq!(derived, epoch + Duration::from_secs(1));
    }

    /// Evenly-spaced RTP offsets must produce evenly-spaced wallclocks. This is the
    /// property that was broken: RTP advanced evenly while wallclock jittered.
    #[test]
    fn evenly_spaced_frames_produce_evenly_spaced_wallclocks() {
        let epoch = Instant::now();
        let far_future = epoch + Duration::from_mins(1);
        let times: Vec<Instant> = (0..5)
            .map(|frame| audio_wallclock(epoch, frame * 960, far_future))
            .collect();
        for pair in times.windows(2) {
            let (Some(a), Some(b)) = (pair.first(), pair.get(1)) else {
                continue;
            };
            assert_eq!(b.saturating_duration_since(*a), Duration::from_millis(20));
        }
    }

    /// A wallclock in the future makes str0m emit a Sender Report with `rtp_time` zero,
    /// so running ahead of real time must clamp rather than report the future.
    #[test]
    fn never_reports_a_wallclock_in_the_future() {
        let epoch = Instant::now();
        // Media timeline claims 10s elapsed, but only 1s of real time has passed.
        let now = epoch + Duration::from_secs(1);
        let derived = audio_wallclock(epoch, 10 * 48_000, now);
        assert_eq!(derived, now, "must clamp to now rather than lead it");
    }

    /// The first frame maps to the epoch exactly.
    #[test]
    fn the_first_frame_maps_to_the_epoch() {
        let epoch = Instant::now();
        let now = epoch + Duration::from_millis(5);
        assert_eq!(audio_wallclock(epoch, 0, now), epoch);
    }
}

#[cfg(test)]
mod audio_send_pacing_tests {
    use super::{AudioSendPacing, rtp_payload_type};
    use std::time::{Duration, Instant};

    /// Minimal 12-byte RTP header: version 2, the given `[marker:1][pt:7]` byte, rest zero.
    fn rtp_raw(byte1: u8) -> Vec<u8> {
        let mut p = vec![0x80u8, byte1];
        p.resize(12, 0);
        p
    }

    #[test]
    fn parses_the_payload_type_of_an_rtp_packet() {
        assert_eq!(rtp_payload_type(&rtp_raw(111)), Some(111));
    }

    #[test]
    fn ignores_the_marker_bit_when_reading_the_payload_type() {
        assert_eq!(rtp_payload_type(&rtp_raw(0x6F | 0x80)), Some(111));
    }

    #[test]
    fn rejects_rtcp_multiplexed_on_the_same_port() {
        // RFC 5761 reserves payload types 64..=95 so RTCP can be demultiplexed; a sender
        // report (PT 200) lands at 200 & 0x7F == 72, inside that range.
        assert_eq!(rtp_payload_type(&rtp_raw(200)), None);
    }

    #[test]
    fn rejects_non_rtp_datagrams() {
        // STUN binding request: version bits are not 2.
        let stun = vec![0x00u8; 20];
        assert_eq!(rtp_payload_type(&stun), None);
        // Truncated packet, shorter than an RTP header.
        assert_eq!(rtp_payload_type(&[0x80, 111]), None);
    }

    #[test]
    fn reports_once_per_window_and_not_before() {
        let mut p = AudioSendPacing::default();
        let start = Instant::now();
        for i in 0..249u64 {
            assert!(p.record(start + Duration::from_millis(i * 20)).is_none());
        }
        assert!(p.record(start + Duration::from_millis(249 * 20)).is_some());
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn an_evenly_paced_stream_reports_a_tight_twenty_millisecond_gap() {
        let mut p = AudioSendPacing::default();
        let start = Instant::now();
        let mut report = None;
        for i in 0..250u64 {
            report = p.record(start + Duration::from_millis(i * 20)).or(report);
        }
        let r = report.expect("a report after 250 packets");
        assert_eq!((r.min_gap_ms, r.mean_gap_ms, r.max_gap_ms), (20, 20, 20));
        assert_eq!((r.clumped, r.late), (0, 0));
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn clumped_sends_are_counted_as_a_back_to_back_pair_plus_a_late_packet() {
        // The signature of the I/O loop holding frames back: two frames leave together,
        // then nothing for 40ms. Average spacing is still a perfect 20ms, which is why
        // a mean alone cannot detect this.
        let mut p = AudioSendPacing::default();
        let start = Instant::now();
        let mut report = None;
        for i in 0..125u64 {
            let base = i * 40;
            report = p.record(start + Duration::from_millis(base)).or(report);
            report = p.record(start + Duration::from_millis(base + 1)).or(report);
        }
        let r = report.expect("a report after 250 packets");
        // 125 pairs -> 125 back-to-back gaps, and 124 stalls between consecutive pairs.
        assert_eq!(r.clumped, 125, "one back-to-back send per pair");
        assert_eq!(r.late, 124, "one 39ms stall between pairs");
        // Still ~20ms on average: the mean alone cannot see this at all.
        assert_eq!(r.mean_gap_ms, 19, "the mean hides it entirely");
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn the_gap_across_a_window_boundary_is_not_lost() {
        let mut p = AudioSendPacing::default();
        let start = Instant::now();
        for i in 0..250u64 {
            p.record(start + Duration::from_millis(i * 20));
        }
        // First packet of the second window arrives 100ms after the last of the first.
        let second = 249 * 20 + 100;
        p.record(start + Duration::from_millis(second));
        for i in 1..249u64 {
            p.record(start + Duration::from_millis(second + i * 20));
        }
        // The 100ms stall must appear in the second window, not be swallowed by the reset.
        let r = p
            .record(start + Duration::from_millis(second + 249 * 20))
            .expect("a report after the second window");
        assert!(r.max_gap_ms >= 100, "max_gap_ms was {}", r.max_gap_ms);
        assert_eq!(r.late, 1);
    }
}

#[cfg(test)]
mod offer_track_id_tests {
    use super::{MediaTrackKey, TransceiverInfo, create_offer, create_peer_connection};
    use str0m::media::{Direction, MediaKind};

    /// The shim path must carry the track id through too.
    ///
    /// This is the same defect as the one below, on the code path that is actually used
    /// for SFU calls: `create_offer` builds transceivers from what the JS shim sends, and
    /// it sent only kind and direction. The audio m-line therefore advertised an msid the
    /// SFU had never seen, matching no `AddTrackRequest`, so our audio RTP arrived and was
    /// forwarded to nobody -- while every counter on our side showed frames being sent and
    /// RTCP reporting zero loss.
    #[test]
    fn a_transceiver_built_from_js_keeps_the_tracks_id() {
        let tc = TransceiverInfo::from_js(
            "audio",
            Some("sendrecv"),
            Some("abc-123".to_owned()),
            None,
        );
        assert_eq!(tc.track_id.as_deref(), Some("abc-123"));
        assert_eq!(tc.kind, MediaKind::Audio);
        assert_eq!(tc.direction, Direction::SendRecv);
    }

    /// A transceiver with no track (a recvonly one, say) is still valid.
    #[test]
    fn a_transceiver_with_no_track_is_still_accepted() {
        let tc = TransceiverInfo::from_js("video", Some("recvonly"), None, None);
        assert!(tc.track_id.is_none());
        assert_eq!(tc.direction, Direction::RecvOnly);
    }

    /// A re-offer that changes nothing must still produce an offer.
    ///
    /// str0m reports "no changes" by returning `None`, and turning that into an error cost
    /// three reconnections in seventy seconds: it reached livekit-client as a failed
    /// negotiation, which tore the session down and rebuilt it.
    #[test]
    #[allow(clippy::expect_used)]
    fn a_re_offer_that_changes_nothing_returns_the_offer_already_in_force() {
        let mut pc = create_peer_connection("reoffer-test".to_owned());
        let tcs = [TransceiverInfo::from_js(
            "audio",
            Some("sendonly"),
            None,
            Some("microphone"),
        )];

        let first = create_offer(&mut pc, &[], &tcs).expect("the first offer is a change");
        assert!(!first.sdp.is_empty());

        // A real re-offer carries no new transceivers -- the shim clears its pending list
        // once an offer has been made -- so str0m has nothing to apply. Passing `tcs` again
        // would *add a second* audio m-line, which is a change and not this case.
        let second = create_offer(&mut pc, &[], &[]).expect("a re-offer must not be an error");
        assert_eq!(
            second.sdp, first.sdp,
            "the offer in force describes the current state, which is unchanged"
        );
    }

    /// With nothing offered yet and nothing to offer, the error is real and must remain.
    #[test]
    fn an_empty_offer_with_no_history_is_still_an_error() {
        let mut pc = create_peer_connection("empty-offer-test".to_owned());
        assert!(create_offer(&mut pc, &[], &[]).is_err());
    }

    /// The camera and the screen share are different tracks and must not share an m-line.
    ///
    /// Both used to resolve to `video/camera`, so the first video transceiver offered
    /// claimed the key and the other track had none. The far end saw the camera picture in
    /// the screen-share tile and a black square where the camera belonged.
    #[test]
    fn a_named_source_decides_which_track_a_transceiver_sends() {
        let camera =
            TransceiverInfo::from_js("video", Some("sendonly"), None, Some("camera"));
        let share =
            TransceiverInfo::from_js("video", Some("sendonly"), None, Some("screen_share"));
        assert_eq!(camera.key, Some(MediaTrackKey::camera()));
        assert_eq!(share.key, Some(MediaTrackKey::screen_share()));
        assert_ne!(camera.key, share.key, "two video tracks, two keys");
    }

    /// A name this build does not know falls back rather than inventing a key: the frontend
    /// can be older than this list, and a track sent on the default m-line beats one that
    /// is never sent at all.
    #[test]
    fn an_unknown_source_falls_back_to_the_kind() {
        let tc = TransceiverInfo::from_js("video", Some("sendonly"), None, Some("hologram"));
        assert_eq!(tc.key, Some(MediaTrackKey::camera()));
    }

    /// A receiving transceiver is nobody's track of ours and must claim no key at all.
    #[test]
    fn a_receiving_transceiver_claims_nothing_even_when_named() {
        let tc = TransceiverInfo::from_js("video", Some("recvonly"), None, Some("camera"));
        assert_eq!(tc.key, None);
    }

    /// `LiveKit`'s SFU pairs an `AddTrackRequest` with an m-line by matching the request's
    /// `cid` against the offer's msid track id. livekit-client gets this for free because
    /// its `cid` *is* the `MediaStreamTrack.id`; we have to pass the value through
    /// explicitly. When it was left unset str0m invented an id the SFU had never seen, and
    /// association fell through to the server's match-by-kind fallback -- which works until
    /// ordering or track counts change, and then silently publishes a track nobody
    /// subscribes to.
    #[test]
    #[allow(clippy::expect_used)]
    fn the_offer_advertises_the_supplied_track_id_as_its_msid() {
        let mut pc = create_peer_connection("test".to_owned());
        let offer = create_offer(
            &mut pc,
            &[],
            &[TransceiverInfo {
                kind: MediaKind::Audio,
                direction: Direction::SendRecv,
                track_id: Some("PA_abc123-audio-microphone".to_owned()),
                key: Some(MediaTrackKey::microphone()),
            }],
        )
        .expect("offer");

        let msid = offer
            .sdp
            .lines()
            .find(|l| l.starts_with("a=msid:"))
            .expect("the offer must carry an msid line");
        assert!(
            msid.ends_with(" PA_abc123-audio-microphone"),
            "msid track id must be the supplied cid, got {msid:?}"
        );
    }

    /// Re-offering must not allocate a second mid for a kind that already has one:
    /// `add_media` appends unconditionally, so publishing video after audio would otherwise
    /// describe the audio track twice and leave the first m-line orphaned.
    ///
    /// Asserts on the recorded mid rather than the SDP: a faithful SDP assertion would need
    /// a completed offer/answer handshake, and the mid is the state the rest of the code
    /// actually uses to route media.
    #[test]
    #[allow(clippy::expect_used)]
    fn re_offering_keeps_the_existing_transceivers_mid() {
        let mut pc = create_peer_connection("test".to_owned());
        let audio = TransceiverInfo {
            kind: MediaKind::Audio,
            direction: Direction::SendRecv,
            track_id: Some("cid-audio".to_owned()),
            key: Some(MediaTrackKey::microphone()),
        };
        create_offer(&mut pc, &[], std::slice::from_ref(&audio)).expect("first offer");
        let first_mid = pc.audio_mid.expect("audio mid recorded");

        create_offer(
            &mut pc,
            &[],
            &[
                audio,
                TransceiverInfo {
                    kind: MediaKind::Video,
                    direction: Direction::SendRecv,
                    track_id: Some("cid-video".to_owned()),
                    key: Some(MediaTrackKey::camera()),
                },
            ],
        )
        .expect("second offer");

        assert_eq!(
            pc.audio_mid,
            Some(first_mid),
            "the audio transceiver must keep its original mid across a renegotiation"
        );
        assert!(
            pc.video_mid.is_some(),
            "the new video transceiver must be added"
        );
        assert_ne!(pc.video_mid, pc.audio_mid);
    }

    /// The offer has to advertise H.264, or none of the H.264 work is reachable.
    ///
    /// Asserted on the SDP rather than on the config, because `enable_h264` returning a
    /// builder proves nothing about what the far end is told. This is the one line that
    /// decides whether a peer may publish H.264 to us at all.
    #[test]
    #[allow(clippy::expect_used)]
    fn the_offer_advertises_both_video_codecs() {
        let mut pc = create_peer_connection("test".to_owned());
        let offer = create_offer(
            &mut pc,
            &[],
            &[TransceiverInfo {
                kind: MediaKind::Video,
                direction: Direction::SendRecv,
                track_id: Some("cid-video".to_owned()),
                key: Some(MediaTrackKey::camera()),
            }],
        )
        .expect("offer");

        let rtpmaps: Vec<&str> = offer
            .sdp
            .lines()
            .filter(|l| l.starts_with("a=rtpmap:"))
            .collect();
        let joined = rtpmaps.join("\n");
        assert!(
            joined.contains("VP8/90000"),
            "VP8 must stay offered; every peer supports it. got:\n{joined}"
        );
        assert!(
            joined.contains("H264/90000"),
            "H.264 must be offered now there is a decoder for it. got:\n{joined}"
        );
    }

    /// livekit protocol 17 receives on the publisher connection: `onMediaSectionsRequirement`
    /// adds one `recvonly` transceiver per remote track and re-offers. Each is a separate
    /// m-line, and dropping any of them leaves the SFU with nowhere to send that stream.
    ///
    /// This failed before the identity check was keyed on the track: the first section was
    /// added, every later one of the same kind was skipped as "already present", the offer
    /// carried a single audio m-line, and no inbound audio was ever decoded while every
    /// outbound counter looked healthy.
    #[test]
    #[allow(clippy::expect_used)]
    fn every_receive_section_asked_for_gets_its_own_m_line() {
        let mut pc = create_peer_connection("test".to_owned());
        let recv = || TransceiverInfo {
            kind: MediaKind::Audio,
            direction: Direction::RecvOnly,
            track_id: None,
            key: None,
        };
        let offer = create_offer(&mut pc, &[], &[recv(), recv(), recv()]).expect("offer");

        let audio_lines = offer
            .sdp
            .lines()
            .filter(|l| l.starts_with("m=audio"))
            .count();
        assert_eq!(
            audio_lines, 3,
            "three receive sections were asked for; the offer must carry three"
        );
    }

    /// Adding a receive section must not move where we send.
    ///
    /// `audio_mid` is the mid `write_audio` publishes on. If a later `recvonly` section
    /// took it over, the microphone would go down a slot the SFU has allocated to another
    /// participant's stream.
    #[test]
    #[allow(clippy::expect_used)]
    fn a_later_receive_section_does_not_take_over_the_send_mid() {
        let mut pc = create_peer_connection("test".to_owned());
        create_offer(
            &mut pc,
            &[],
            &[TransceiverInfo {
                kind: MediaKind::Audio,
                direction: Direction::SendRecv,
                track_id: Some("cid-audio".to_owned()),
                key: Some(MediaTrackKey::microphone()),
            }],
        )
        .expect("first offer");
        let send_mid = pc.audio_mid.expect("audio mid recorded");

        create_offer(
            &mut pc,
            &[],
            &[TransceiverInfo {
                kind: MediaKind::Audio,
                direction: Direction::RecvOnly,
                track_id: None,
                key: None,
            }],
        )
        .expect("second offer");

        assert_eq!(
            pc.audio_mid,
            Some(send_mid),
            "the send mid must not move when a receive section is added"
        );
    }
}

#[cfg(test)]
mod ice_disconnect_tests {
    use super::{
        ICE_DISCONNECT_GRACE, IceState, create_peer_connection, ice_disconnect_expired,
        note_ice_state,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn a_connection_that_was_never_disconnected_never_expires() {
        assert!(!ice_disconnect_expired(None, Instant::now()));
    }

    #[test]
    fn a_brief_disconnection_does_not_expire() {
        let since = Instant::now();
        assert!(!ice_disconnect_expired(
            Some(since),
            since + ICE_DISCONNECT_GRACE.saturating_sub(Duration::from_millis(1))
        ));
    }

    #[test]
    fn a_disconnection_past_the_grace_period_expires() {
        let since = Instant::now();
        assert!(ice_disconnect_expired(
            Some(since),
            since + ICE_DISCONNECT_GRACE
        ));
    }

    /// The whole point: `Disconnected` must not kill the connection on its own.
    ///
    /// str0m documents this state as recoverable and has no terminal failure state at all
    /// ("it's always possible to come back"). Treating it as fatal made every transient
    /// blip permanent.
    #[test]
    fn disconnecting_does_not_kill_the_connection() {
        let mut pc = create_peer_connection("test".to_owned());
        note_ice_state(&mut pc, IceState::Disconnected);
        assert!(
            pc.alive,
            "a disconnection must not mark the connection dead"
        );
        assert!(pc.ice_disconnected_since.is_some(), "the clock must start");
    }

    #[test]
    fn recovering_clears_the_disconnection_clock() {
        let mut pc = create_peer_connection("test".to_owned());
        note_ice_state(&mut pc, IceState::Disconnected);
        note_ice_state(&mut pc, IceState::Connected);
        assert!(pc.ice_disconnected_since.is_none());
        assert!(pc.alive);
    }

    /// Repeated `Disconnected` events must not keep resetting the clock, or a connection
    /// flapping every few seconds would never exhaust the grace period.
    #[test]
    fn repeated_disconnections_do_not_restart_the_clock() {
        let mut pc = create_peer_connection("test".to_owned());
        note_ice_state(&mut pc, IceState::Disconnected);
        let first = pc.ice_disconnected_since;
        note_ice_state(&mut pc, IceState::Disconnected);
        assert_eq!(pc.ice_disconnected_since, first);
    }
}

#[cfg(test)]
mod local_candidate_tests {
    use super::local_host_addresses;

    /// A candidate a peer cannot route to is worse than useless: it consumes connectivity
    /// checks while the pair that would have worked waits behind it.
    #[test]
    fn loopback_and_link_local_are_not_advertised() {
        for ip in local_host_addresses() {
            assert!(!ip.is_loopback(), "loopback must not be advertised: {ip}");
            match ip {
                std::net::IpAddr::V4(v4) => {
                    assert!(
                        !v4.is_link_local(),
                        "link-local must not be advertised: {v4}"
                    );
                }
                std::net::IpAddr::V6(v6) => {
                    assert!(
                        !v6.is_unicast_link_local(),
                        "link-local must not be advertised: {v6}"
                    );
                }
            }
        }
    }

    /// The whole point of the change: a machine with more than one interface must offer
    /// more than one candidate, because the one that routes to the internet is frequently
    /// not the one a peer on the same LAN (or an SFU in a container) can reach.
    #[test]
    fn at_least_one_address_is_found_on_a_networked_host() {
        assert!(
            !local_host_addresses().is_empty(),
            "a host with any usable interface must advertise at least one candidate"
        );
    }
}
