//! `LiveKit` room state management.
//!
//! Manages the connection lifecycle, participant state, track publishing/subscribing,
//! and bridges signaling messages to the dual `PeerConnection` transport.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tracing::Instrument;

use livekit_protocol::signal_request;
use livekit_protocol::signal_response;
use livekit_protocol::{
    AddTrackRequest, JoinResponse, ParticipantInfo, SessionDescription as LkSessionDescription,
    SignalTarget, TrackSource, TrackType, encryption,
};

use elementium_types::{CorrelationId, MediaTrackKey, SdpType, SessionDescription, TrackKind};

use crate::e2ee_io::EncryptionPolicy;
use crate::engine::{IoCommand, VideoFrameBuffer};
use crate::livekit::signaling::{SignalClient, SignalSender};
use crate::livekit::transport::{Transport, TransportCommand, TransportEvent};
use crate::peer_connection::PcEvent;

/// Events emitted by the room to the Tauri layer.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum RoomEvent {
    #[serde(rename_all = "camelCase")]
    ParticipantJoined {
        room_id: String,
        identity: String,
        sid: String,
        name: String,
    },
    #[serde(rename_all = "camelCase")]
    ParticipantLeft {
        room_id: String,
        identity: String,
        sid: String,
    },
    #[serde(rename_all = "camelCase")]
    TrackSubscribed {
        room_id: String,
        /// Empty until resolved from the signalling messages; never a placeholder string.
        participant_sid: String,
        /// The SFU's track sid, empty until resolved. Distinct from `mid`, which is ours.
        track_sid: String,
        /// The m-line the track arrived on, which is the identifier we actually have.
        mid: String,
        kind: String,
    },
    #[serde(rename_all = "camelCase")]
    TrackUnsubscribed {
        room_id: String,
        participant_sid: String,
        track_sid: String,
    },
    #[serde(rename_all = "camelCase")]
    ConnectionStateChanged { room_id: String, state: String },
    #[serde(rename_all = "camelCase")]
    ActiveSpeakersChanged {
        room_id: String,
        speakers: Vec<String>,
    },
    /// The SFU has told us which codec subscribers can actually take.
    ///
    /// Emitted when that changes mid-call -- typically because a participant joined who
    /// cannot decode what everyone else could. The capture pipeline follows this rather
    /// than deciding for itself, because only the SFU knows who is subscribed.
    #[serde(rename_all = "camelCase")]
    /// The SFU has told us which codec its subscribers are taking.
    ///
    /// `encrypted` travels with it because the answer is not the same in both cases: our
    /// H.264 under E2EE is decoded by another Elementium peer and by two browsers talking
    /// to each other, but not by a browser receiving from us. Acting on this event without
    /// knowing whether the room is encrypted would swap a working stream for a black tile.
    PublishCodecChanged {
        room_id: String,
        codec: String,
        encrypted: bool,
    },
}

/// What to put in `AddTrackRequest.encryption` for a room running under `policy`.
///
/// Derived from the policy rather than passed in separately, so the field that tells
/// subscribers how to treat our frames cannot disagree with what the transport does to
/// them. Our E2EE is AES-GCM in `LiveKit`'s own frame layout, so `Gcm` is the accurate
/// description -- `Custom` would tell subscribers to expect a scheme we do not use.
const fn declared_encryption(policy: &EncryptionPolicy) -> encryption::Type {
    if policy.as_context().is_some() {
        encryption::Type::Gcm
    } else {
        encryption::Type::None
    }
}

/// The `LiveKit` room manages signaling, transport, and participant state.
pub struct LiveKitRoom {
    pub room_id: String,
    pub room_name: String,
    pub local_identity: String,
    pub local_sid: String,
    signal_sender: SignalSender,
    signal_client: SignalClient,
    transport: Transport,
    participants: HashMap<String, ParticipantInfo>,
    /// Negotiation state shared with the signal loop. See [`PublishState`].
    publish_state: Arc<Mutex<PublishState>>,
    room_event_tx: mpsc::UnboundedSender<RoomEvent>,
    video_frames: VideoFrameBuffer,
    /// Where the capture pipelines write encoded media for this room.
    ///
    /// The mic and camera threads speak [`IoCommand`], because they were written against
    /// the direct-`WebRTC` engine; the SFU transport speaks [`TransportCommand`]. Without
    /// something joining the two, a capture pipeline can run perfectly -- encoding,
    /// logging frame counts, showing a local preview -- while nothing it produces ever
    /// reaches the socket, which is exactly the shape of "the camera light is on but
    /// nobody sees me".
    media_tx: mpsc::Sender<IoCommand>,
    shutdown: bool,
    correlation_id: CorrelationId,
    /// How every track this room publishes is described to the SFU.
    ///
    /// Must agree with what the transport actually does to the frames. A track encrypted
    /// with our E2EE key but announced as `NONE` is relayed to subscribers who were told
    /// the payload is plaintext -- they hand ciphertext straight to Opus or VP8 and get
    /// noise or nothing, with no error anywhere to say why.
    track_encryption: encryption::Type,
}

/// What the publisher has announced, and whether an offer is outstanding.
///
/// Shared between `publish_track` and the signal loop because they are the two halves of
/// one conversation: one side starts an offer, the other applies the answer, and only the
/// second knows when the first may start another.
#[derive(Debug, Default)]
struct PublishState {
    /// The `cid` announced for each track published so far, in publish order.
    ///
    /// Ordered rather than a map because an offer's m-lines are positional: an established
    /// track has to keep the section it was first given, and a `HashMap`'s iteration order
    /// would reshuffle them on every renegotiation. Each answer would then describe
    /// different m-lines than the offer it replies to.
    published: Vec<(MediaTrackKey, String)>,
    /// The server-assigned track sid for each published track, once the SFU confirms it.
    ///
    /// Kept because muting is addressed by sid, not by cid: the cid is ours and means
    /// nothing to the SFU after publication. Without this a user muting their microphone
    /// changed nothing anyone else could see -- the mute stayed local, every other
    /// participant's UI kept showing them unmuted, and the SFU kept forwarding.
    sids: HashMap<MediaTrackKey, String>,
    /// An offer has been sent and its answer has not been applied.
    ///
    /// str0m keeps exactly one pending offer. Creating a second one while the first is
    /// unanswered replaces it, and the first answer then arrives describing m-lines the
    /// surviving offer never had -- "Mid in answer is not in offer". Worse, the second
    /// offer describes only the newly added track, so the answer for it drops the other
    /// one too, and a publisher that had working audio ends up sending nothing at all.
    offer_in_flight: bool,
    /// A track was published while an offer was outstanding, so another offer is owed once
    /// the current one completes.
    renegotiate_pending: bool,
}

impl PublishState {
    /// Record a track as published, or return the `cid` it already has.
    ///
    /// Re-publishing the same track must not add a second m-line for it: the SFU pairs by
    /// `cid`, so a duplicate would leave two sections claiming the same track and the
    /// association would depend on which the server matched first.
    fn announce(&mut self, key: MediaTrackKey, cid: String) {
        if !self.published.iter().any(|(k, _)| *k == key) {
            self.published.push((key, cid));
        }
    }
}

/// The transceivers an offer should describe, given what has been published so far.
///
/// Always every published track, never just the new one: an offer is a complete description
/// of the connection, and one that omits an established track asks the far end to drop it.
fn publish_transceivers(state: &PublishState) -> Vec<crate::peer_connection::TransceiverInfo> {
    state
        .published
        .iter()
        .map(|(key, cid)| crate::peer_connection::TransceiverInfo {
            kind: match key.kind() {
                TrackKind::Audio => str0m::media::MediaKind::Audio,
                TrackKind::Video => str0m::media::MediaKind::Video,
            },
            direction: str0m::media::Direction::SendRecv,
            track_id: Some(cid.clone()),
            // The whole point of the exercise: this is what lets a later write find this
            // m-line again instead of guessing that "the video one" means the camera.
            key: Some(*key),
        })
        .collect()
}

/// Build an offer from the publisher connection and send it.
///
/// Shared by the first publish and by the renegotiation the signal loop starts once an
/// answer lands, so both produce the same shape of offer from the same state.
fn send_publisher_offer(
    publisher: &crate::peer_connection::PeerConnectionHandle,
    signal_sender: &SignalSender,
    state: &PublishState,
) -> Result<(), crate::error::WebRtcError> {
    let transceivers = publish_transceivers(state);
    let offer = {
        let mut pc = publisher
            .lock()
            .map_err(|_| crate::error::WebRtcError::LockPoisoned)?;
        crate::peer_connection::create_offer(&mut pc, &[], &transceivers)?
    };
    tracing::info!(
        offer_mids = %mid_list(&offer.sdp),
        tracks = %state
            .published
            .iter()
            .map(|(k, _)| k.to_string())
            .collect::<Vec<_>>()
            .join(","),
        "publisher offer"
    );
    signal_sender
        .send(signal_request::Message::Offer(LkSessionDescription {
            r#type: "offer".to_owned(),
            sdp: offer.sdp,
            ..Default::default()
        }))
        .map_err(|e| {
            crate::error::WebRtcError::Signaling(format!("failed to send publisher offer: {e}"))
        })
}

/// The `a=mid:` values in an SDP, in order, for logging a negotiation mismatch.
///
/// "Mid in answer is not in offer" names one mid and neither list, which leaves the actual
/// question -- which m-lines did each side describe -- unanswerable from the log.
fn mid_list(sdp: &str) -> String {
    sdp.lines()
        .filter_map(|l| l.strip_prefix("a=mid:"))
        .collect::<Vec<_>>()
        .join(",")
}

impl LiveKitRoom {
    /// Correlation ID for this room's session, minted by the caller at connect time.
    ///
    /// Reused to scope log spans for later operations on this room (disconnect,
    /// track publish, etc.) so they share the same session's `correlation_id`.
    #[must_use]
    pub const fn correlation_id(&self) -> &CorrelationId {
        &self.correlation_id
    }

    /// Connect to a `LiveKit` SFU room.
    ///
    /// 1. Opens WebSocket signaling connection
    /// 2. Waits for `JoinResponse`
    /// 3. Creates dual `PeerConnection` transport
    /// 4. Starts the signal processing loop
    ///
    /// Returns the room and a receiver for room events.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the signaling connection fails, the `JoinResponse` is
    /// never received (or times out), or the transport cannot be created.
    // Structured connect-attempt/failure logging at each fallible step, plus session
    // span setup, adds lines without adding branching complexity worth splitting out.
    #[allow(clippy::too_many_lines)]
    pub async fn connect(
        sfu_url: &str,
        token: &str,
        video_frames: VideoFrameBuffer,
        correlation_id: CorrelationId,
        e2ee: EncryptionPolicy,
    ) -> Result<(Self, mpsc::UnboundedReceiver<RoomEvent>), crate::error::WebRtcError> {
        let room_id = generate_room_id();

        // Connect signaling. `connect()` hands back the receiver directly rather than
        // stashing it behind an `Option` a caller then has to `take()` -- see X7 in
        // specs/BACKLOG-2026-08-09-errors.md: the previous shape modelled "the receiver
        // was already taken" as a `ChannelClosed` runtime error, immediately after a
        // freshly-constructed `SignalClient` that nothing else could have touched. That
        // was an invariant, not a possible failure, and is now expressed by the type
        // signature instead of a branch that could never be taken.
        let (signal_client, mut signal_rx) = match SignalClient::connect(sfu_url, token).await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(reason = %e, "signaling connect failed");
                return Err(crate::error::WebRtcError::Signaling(format!(
                    "connect failed: {e}"
                )));
            }
        };

        let signal_sender = signal_client.sender();

        // Wait for JoinResponse
        let join_response = match wait_for_join(&mut signal_rx).await {
            Ok(j) => j,
            Err(e) => {
                tracing::error!(reason = %e, "connect attempt failed");
                return Err(e);
            }
        };

        let room_name = join_response
            .room
            .as_ref()
            .map(|r| r.name.clone())
            .unwrap_or_default();
        let local_identity = join_response
            .participant
            .as_ref()
            .map(|p| p.identity.clone())
            .unwrap_or_default();
        let local_sid = join_response
            .participant
            .as_ref()
            .map(|p| p.sid.clone())
            .unwrap_or_default();

        // What the SFU will accept from us. Offering a codec it has not enabled wastes a
        // negotiation round at best, and at worst has it forward a stream no subscriber
        // asked for. An empty list means the server did not say.
        let server_codecs: Vec<elementium_codec::VideoCodec> = join_response
            .enabled_publish_codecs
            .iter()
            .filter_map(|c| elementium_codec::VideoCodec::from_mime(&c.mime))
            .collect();
        tracing::info!(
            server_codecs = ?server_codecs.iter().map(|c| c.sdp_name()).collect::<Vec<_>>(),
            raw = ?join_response.enabled_publish_codecs.iter().map(|c| c.mime.as_str()).collect::<Vec<_>>(),
            "SFU published its enabled codecs"
        );

        tracing::info!(
            room_id = %room_id,
            room_name = %room_name,
            local_identity = %local_identity,
            "connected"
        );

        // Build initial participant list
        let mut participants = HashMap::new();
        for p in &join_response.other_participants {
            participants.insert(p.sid.clone(), p.clone());
        }

        // What we will tell the SFU about every track we publish. Decided here, before the
        // policy is moved into the transport, so the declaration and the behaviour cannot
        // drift apart: whatever encrypts the frames is what describes them.
        let track_encryption = declared_encryption(&e2ee);

        // Shared with the signal loop, which is where an answer is applied and therefore
        // the only place that knows when the next offer may be sent.
        let publish_state = Arc::new(Mutex::new(PublishState::default()));

        // Create transport (Publisher + Subscriber PeerConnections)
        let transport = match Transport::new_with_e2ee(&room_id, e2ee, None) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(reason = %e, "connect attempt failed");
                return Err(e);
            }
        };

        // Room event channel
        let (room_event_tx, room_event_rx) = mpsc::unbounded_channel();

        // Emit initial participants
        for p in participants.values() {
            send_room_event(
                &room_event_tx,
                RoomEvent::ParticipantJoined {
                    room_id: room_id.clone(),
                    identity: p.identity.clone(),
                    sid: p.sid.clone(),
                    name: p.name.clone(),
                },
            );
        }

        send_room_event(
            &room_event_tx,
            RoomEvent::ConnectionStateChanged {
                room_id: room_id.clone(),
                state: "connected".to_string(),
            },
        );

        let media_tx = spawn_media_forwarder(transport.cmd_tx.clone());

        let room = Self {
            room_id: room_id.clone(),
            room_name,
            local_identity,
            local_sid,
            signal_sender: signal_sender.clone(),
            signal_client,
            transport,
            participants,
            publish_state: Arc::clone(&publish_state),
            room_event_tx: room_event_tx.clone(),
            video_frames,
            media_tx,
            shutdown: false,
            correlation_id,
            track_encryption,
        };

        // Spawn signal processing loop, instrumented with the ambient session
        // span so its events (and the transport-event blocking task it spawns)
        // keep the same correlation_id.
        let sig_sender = signal_sender;
        let publisher_handle = room.transport.publisher.clone();
        let subscriber_handle = room.transport.subscriber.clone();
        let transport_event_rx = room.transport.event_rx.clone();
        let vf = room.video_frames.clone();
        let rid = room_id.clone();
        let evt_tx = room_event_tx;
        let loop_publish_state = Arc::clone(&publish_state);
        let room_is_encrypted = track_encryption != encryption::Type::None;
        let loop_span = tracing::Span::current();

        tokio::spawn(
            async move {
                signal_processing_loop(
                    signal_rx,
                    sig_sender,
                    publisher_handle,
                    subscriber_handle,
                    transport_event_rx,
                    vf,
                    rid,
                    evt_tx,
                    loop_publish_state,
                    room_is_encrypted,
                )
                .await;
            }
            .instrument(loop_span),
        );

        Ok((room, room_event_rx))
    }

    /// Publish a local track (audio or video) to the SFU.
    ///
    /// Sends an `AddTrackRequest` to the SFU, then triggers an SDP renegotiation
    /// on the Publisher `PeerConnection`.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `kind` is unrecognized, or if sending the `AddTrack`
    /// request, creating the publisher offer, or sending the offer fails.
    /// Publish a video track, declaring the codec and size it will actually carry.
    ///
    /// The SFU registers a video track as `video/VP8` unless told otherwise, and then
    /// expects VP8 on the wire. Publishing H.264 against that declaration produces a track
    /// the SFU accepts, counts, and never forwards decodably -- the publisher's own
    /// counters show frames going out and no subscriber ever sees a picture.
    ///
    /// # Errors
    ///
    /// As [`LiveKitRoom::publish_track`].
    pub fn publish_video_track(
        &mut self,
        key: MediaTrackKey,
        codec: elementium_codec::VideoCodec,
        width: u32,
        height: u32,
    ) -> Result<(), crate::error::WebRtcError> {
        self.publish_track_inner(key, Some((codec, width, height)))
    }

    /// Publish a track by kind, without declaring a codec.
    ///
    /// Audio only, in practice: video needs [`LiveKitRoom::publish_video_track`], because a
    /// video track the SFU is not told the codec of is registered as VP8.
    ///
    /// Tell the SFU that one of our published tracks is muted, or is muted no longer.
    ///
    /// Muting is a signalling fact, not a local one. A client that simply stops sending
    /// leaves every other participant's UI showing it unmuted, and leaves the SFU expecting
    /// media that never comes -- so the far end shows a live camera tile that has quietly
    /// frozen, which is indistinguishable from a network stall. `MuteTrackRequest` is how
    /// every other `LiveKit` client says it, and the SFU relays it to the room.
    ///
    /// Addressed by the server-assigned sid rather than our `cid`: the cid identifies the
    /// track only in the `AddTrack` exchange that created it.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::WebRtcError::NoMidForTrack`] if the track was never
    /// published, or has not yet been confirmed by the SFU -- there is nothing to mute and
    /// silently succeeding would be a lie the caller would act on.
    pub fn set_track_muted(
        &self,
        key: MediaTrackKey,
        muted: bool,
    ) -> Result<(), crate::error::WebRtcError> {
        let sid = {
            let state = self
                .publish_state
                .lock()
                .map_err(|_| crate::error::WebRtcError::LockPoisoned)?;
            state.sids.get(&key).cloned()
        };
        let Some(sid) = sid else {
            return Err(crate::error::WebRtcError::NoMidForTrack(key.to_string()));
        };

        tracing::info!(track = %key, sid = %sid, muted, "telling the SFU a track's mute state");
        self.signal_sender
            .send(signal_request::Message::Mute(
                livekit_protocol::MuteTrackRequest { sid, muted },
            ))
            .map_err(|e| crate::error::WebRtcError::Signaling(e.to_string()))
    }

    /// # Errors
    ///
    /// Returns an error if the `AddTrack` request or the offer cannot be sent.
    pub fn publish_track(&mut self, key: MediaTrackKey) -> Result<(), crate::error::WebRtcError> {
        self.publish_track_inner(key, None)
    }

    // `&self`: publishing changes only the shared `publish_state` and the signalling
    // socket, both behind their own synchronisation. It stopped needing exclusive access
    // when the room's duplicate `audio_cid`/`video_cid` fields went away.
    fn publish_track_inner(
        &self,
        key: MediaTrackKey,
        video: Option<(elementium_codec::VideoCodec, u32, u32)>,
    ) -> Result<(), crate::error::WebRtcError> {
        let (kind, source) = (key.kind().as_str(), key.source().as_str());
        let track_type: i32 = match key.kind() {
            TrackKind::Audio => TrackType::Audio.into(),
            TrackKind::Video => TrackType::Video.into(),
        };

        let track_source: i32 = match key.source() {
            elementium_types::TrackSource::Microphone => TrackSource::Microphone.into(),
            elementium_types::TrackSource::Camera => TrackSource::Camera.into(),
            elementium_types::TrackSource::ScreenShare => TrackSource::ScreenShare.into(),
            elementium_types::TrackSource::ScreenShareAudio => TrackSource::ScreenShareAudio.into(),
        };

        // The SFU pairs this request with an m-line by matching `cid` against the offer's
        // msid track id, so the same value has to reach both. livekit-client gets this for
        // free (its cid *is* the MediaStreamTrack id); we have to thread it through
        // explicitly, or the association falls back to the server's match-by-kind guess and
        // becomes order-dependent.
        //
        // The tail comes from the key so that the value we route on locally and the value
        // the SFU pairs on are derived from the same thing and cannot diverge.
        let cid = format!("{}-{}", self.local_sid, key.cid_suffix());

        let span = tracing::info_span!("session", correlation_id = %self.correlation_id, room_id = %self.room_id);
        let _guard = span.enter();

        tracing::info!(kind = kind, source = source, "publishing track");

        // Send AddTrack request
        #[allow(deprecated)]
        if let Err(e) =
            self.signal_sender
                .send(signal_request::Message::AddTrack(AddTrackRequest {
                    cid: cid.clone(),
                    name: format!("{kind}_{source}"),
                    r#type: track_type,
                    source: track_source,
                    // The size we actually publish, when the caller knows it. The old
                    // hardcoded 640x480 was announced for every video track regardless, so
                    // the SFU's view of the stream disagreed with the stream.
                    width: video.map_or(0, |(_, w, _)| w),
                    height: video.map_or(0, |(_, _, h)| h),
                    // The codec, for the same reason. Without this the SFU assumes VP8.
                    simulcast_codecs: video
                        .map(|(codec, _, _)| {
                            vec![livekit_protocol::SimulcastCodec {
                                codec: codec.sdp_name().to_lowercase(),
                                cid: cid.clone(),
                                ..Default::default()
                            }]
                        })
                        .unwrap_or_default(),
                    // How the SFU should handle a subscriber that cannot decode our codec.
                    //
                    // The case this exists for: a room where everyone supports AV1 gains a
                    // participant who does not. The SFU does not transcode, so either that
                    // participant sees nothing, or the publisher moves to a codec they can
                    // decode. `PreferRegression` asks for the second -- one stream, changed to
                    // suit the room.
                    //
                    // Not `Simulcast`, which would encode both codecs at once: that doubles
                    // the encoding cost for the whole call to accommodate one participant, and
                    // encoding cost is the reason for choosing a hardware codec in the first
                    // place.
                    backup_codec_policy: livekit_protocol::BackupCodecPolicy::PreferRegression
                        .into(),
                    // Subscribers decide whether to decrypt from this, not from anything in the
                    // media itself. Defaulting it left every E2EE track announced as plaintext.
                    encryption: self.track_encryption.into(),
                    ..Default::default()
                }))
        {
            tracing::error!(reason = %e, "publish track failed");
            return Err(crate::error::WebRtcError::Signaling(format!(
                "failed to send AddTrack: {e}"
            )));
        }

        tracing::info!(cid = %cid, kind, source, "AddTrack cid, also used as the offer msid track id");

        let mut state = self
            .publish_state
            .lock()
            .map_err(|_| crate::error::WebRtcError::LockPoisoned)?;
        state.announce(key, cid);

        // Renegotiation is one at a time. Publishing a second track while the first offer
        // is still unanswered used to replace str0m's pending offer, so the first answer
        // arrived describing m-lines the surviving offer never had and *both* tracks
        // stopped writing. The owed offer is sent when the outstanding answer lands.
        if state.offer_in_flight {
            state.renegotiate_pending = true;
            tracing::info!(
                kind,
                "an offer is already outstanding; renegotiation deferred until it is answered"
            );
            return Ok(());
        }
        state.offer_in_flight = true;
        let result = send_publisher_offer(&self.transport.publisher, &self.signal_sender, &state);
        if result.is_err() {
            // Nothing is outstanding if the offer never went out, and leaving the flag set
            // would block every later publish on this connection.
            state.offer_in_flight = false;
        }
        result
    }

    /// The channel a capture pipeline should write encoded media into to publish it.
    ///
    /// Handing out a sender rather than requiring callers to hold the room lock matters:
    /// the mic produces a frame every 20ms and the camera one every ~16ms, and both run on
    /// blocking threads that must not contend with signaling for the room's mutex.
    #[must_use]
    pub fn media_sender(&self) -> mpsc::Sender<IoCommand> {
        self.media_tx.clone()
    }


    /// Send a media command to the transport (audio/video data).
    ///
    /// # Errors
    ///
    /// Returns `Err` if the command cannot be sent to the transport.
    pub async fn write_audio(
        &self,
        key: MediaTrackKey,
        data: elementium_types::PlaintextMedia,
    ) -> Result<(), crate::error::WebRtcError> {
        self.transport
            .send_command(TransportCommand::WriteAudio(key, data))
            .await
    }

    /// # Errors
    ///
    /// Returns `Err` if the command cannot be sent to the transport.
    pub async fn write_video(
        &self,
        key: MediaTrackKey,
        data: elementium_types::PlaintextMedia,
        codec: elementium_codec::VideoCodec,
    ) -> Result<(), crate::error::WebRtcError> {
        self.transport
            .send_command(TransportCommand::WriteVideo(key, data, codec))
            .await
    }

    /// Get the list of current participants.
    #[must_use]
    pub fn participants(&self) -> Vec<&ParticipantInfo> {
        self.participants.values().collect()
    }

    /// Disconnect from the room.
    pub async fn disconnect(&mut self) {
        if self.shutdown {
            return;
        }
        self.shutdown = true;

        send_room_event(
            &self.room_event_tx,
            RoomEvent::ConnectionStateChanged {
                room_id: self.room_id.clone(),
                state: "disconnected".to_string(),
            },
        );

        self.transport.shutdown().await;
        self.signal_client.disconnect().await;
    }
}

/// Bridge the capture pipelines' [`IoCommand`] channel onto the transport's command
/// channel, returning the sender to hand to those pipelines.
///
/// Both halves already existed and neither reached the other: the pipelines were written
/// for the direct-`WebRTC` engine and are attached to it when a shim `PeerConnection`
/// creates an offer, but an SFU call never goes through that code path. Encoded audio and
/// video were therefore produced and dropped for the whole of every `LiveKit` call.
///
/// The counters are what makes the absence of this bridge visible next time: a publish
/// that never sends logs nothing at all, whereas a stalled forwarder logs a rising
/// `dropped` count.
fn spawn_media_forwarder(cmd_tx: mpsc::Sender<TransportCommand>) -> mpsc::Sender<IoCommand> {
    // Deep enough to absorb a scheduling hiccup in the transport loop (~5s of audio or
    // ~4s of video), shallow enough that a genuinely stuck transport drops fresh media
    // rather than playing out minutes of stale frames once it recovers.
    let (media_tx, mut media_rx) = mpsc::channel::<IoCommand>(256);
    let span = tracing::Span::current();

    tokio::spawn(
        async move {
            let mut forwarded: u64 = 0;
            let mut dropped: u64 = 0;

            while let Some(cmd) = media_rx.recv().await {
                let transport_cmd = match cmd {
                    IoCommand::WriteAudio(key, data) => TransportCommand::WriteAudio(key, data),
                    IoCommand::WriteVideo(key, data, codec) => {
                        TransportCommand::WriteVideo(key, data, codec)
                    }
                    IoCommand::Shutdown => break,
                };

                // `try_send` rather than `send`: blocking here would apply backpressure all
                // the way to the capture thread, which cannot pause a live microphone. If
                // the transport is not keeping up, the honest outcome is a dropped frame
                // and a counter that says so.
                match cmd_tx.try_send(transport_cmd) {
                    Ok(()) => {
                        forwarded = forwarded.saturating_add(1);
                        if forwarded == 1 || forwarded.is_multiple_of(500) {
                            tracing::info!(forwarded, dropped, "capture media forwarded to SFU");
                        }
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        dropped = dropped.saturating_add(1);
                        if dropped.is_multiple_of(50) {
                            tracing::warn!(
                                forwarded,
                                dropped,
                                "transport command channel full, dropping captured media"
                            );
                        }
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
            }

            tracing::info!(forwarded, dropped, "capture media forwarder ended");
        }
        .instrument(span),
    );

    media_tx
}

/// Send a room event to the Tauri layer, logging a structured tracing event
/// first so every `RoomEvent` variant (including ones with no other log call
/// site, such as `TrackUnsubscribed`) is observable in the session's log
/// stream under its `correlation_id`.
fn send_room_event(tx: &mpsc::UnboundedSender<RoomEvent>, event: RoomEvent) {
    match &event {
        RoomEvent::ParticipantJoined {
            room_id,
            identity,
            sid,
            ..
        } => {
            tracing::info!(room_id = %room_id, identity = %identity, sid = %sid, "participant joined");
        }
        RoomEvent::ParticipantLeft {
            room_id,
            identity,
            sid,
        } => {
            tracing::info!(room_id = %room_id, identity = %identity, sid = %sid, "participant left");
        }
        RoomEvent::TrackSubscribed {
            room_id,
            participant_sid,
            track_sid,
            mid,
            kind,
        } => {
            tracing::info!(
                room_id = %room_id,
                participant_sid = %participant_sid,
                track_sid = %track_sid,
                mid = %mid,
                kind = %kind,
                "track subscribed"
            );
        }
        RoomEvent::TrackUnsubscribed {
            room_id,
            participant_sid,
            track_sid,
        } => {
            tracing::info!(
                room_id = %room_id,
                participant_sid = %participant_sid,
                track_sid = %track_sid,
                "track unsubscribed"
            );
        }
        RoomEvent::ConnectionStateChanged { room_id, state } => {
            tracing::info!(room_id = %room_id, state = %state, "connection state changed");
        }
        RoomEvent::PublishCodecChanged {
            room_id,
            codec,
            encrypted,
        } => {
            tracing::info!(room_id, codec, encrypted, "publish codec changed by the SFU");
        }
        RoomEvent::ActiveSpeakersChanged { room_id, speakers } => {
            tracing::debug!(room_id = %room_id, speakers = ?speakers, "active speakers changed");
        }
    }
    // A failed send means the receiver has been dropped, so the frontend is no longer
    // being told anything about this room -- participants joining and leaving, active
    // speakers, track publications. The UI then freezes on whatever it last knew, which
    // looks like a call where nobody else is doing anything.
    if tx.send(event).is_err() {
        tracing::warn!("room events have nowhere to go; the UI will not see this one");
    }
}

/// Wait for the `JoinResponse` from the signaling channel.
async fn wait_for_join(
    rx: &mut mpsc::UnboundedReceiver<signal_response::Message>,
) -> Result<JoinResponse, crate::error::WebRtcError> {
    let timeout = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(msg) = rx.recv().await {
            if let signal_response::Message::Join(join) = msg {
                return Ok(join);
            }
            tracing::debug!("Ignoring pre-join message");
        }
        Err(crate::error::WebRtcError::ChannelClosed(
            "signaling (before JoinResponse)",
        ))
    });

    timeout
        .await
        .unwrap_or(Err(crate::error::WebRtcError::Timeout("JoinResponse")))
}

/// Background loop: processes signaling messages and transport events.
#[allow(clippy::too_many_arguments)]
// Splitting this match-driven event loop would scatter the signaling state machine
// across multiple functions with no behavioral benefit; length comes from an exhaustive match.
#[allow(clippy::too_many_lines)]
async fn signal_processing_loop(
    mut signal_rx: mpsc::UnboundedReceiver<signal_response::Message>,
    signal_sender: SignalSender,
    publisher: crate::peer_connection::PeerConnectionHandle,
    subscriber: crate::peer_connection::PeerConnectionHandle,
    transport_event_rx: Arc<Mutex<mpsc::Receiver<TransportEvent>>>,
    video_frames: VideoFrameBuffer,
    room_id: String,
    event_tx: mpsc::UnboundedSender<RoomEvent>,
    publish_state: Arc<Mutex<PublishState>>,
    encrypted: bool,
) {
    // Spawn a blocking task to process transport events (audio/video from subscriber PC).
    // Must be blocking because AudioPlayer (cpal) is not Send. spawn_blocking doesn't
    // propagate the ambient span on its own, so capture it and enter it inside the
    // closure to keep the same correlation_id on its events.
    let vf = video_frames;
    let rid = room_id.clone();
    let evt = event_tx.clone();
    let transport_events_span = tracing::Span::current();
    tokio::task::spawn_blocking(move || {
        let _guard = transport_events_span.enter();
        process_transport_events(&transport_event_rx, &vf, &rid, &evt);
    });

    // Process signaling messages
    while let Some(msg) = signal_rx.recv().await {
        match msg {
            signal_response::Message::Answer(answer) => {
                // SFU answer for our Publisher offer. Logged on arrival and on application:
                // without this, a publisher that never completes negotiation is silent in
                // the log, and looks identical to one whose media is being dropped later.
                tracing::info!(
                    sdp_len = answer.sdp.len(),
                    "publisher answer received from SFU"
                );
                let desc = SessionDescription {
                    sdp_type: SdpType::Answer,
                    sdp: answer.sdp,
                };
                let Ok(mut pc) = publisher.lock() else {
                    tracing::error!(
                        reason = "lock_poisoned",
                        "publisher answer dropped; the publisher connection cannot complete \
                         negotiation and will never send media"
                    );
                    continue;
                };
                match crate::peer_connection::set_remote_description(&mut pc, &desc) {
                    Ok(_) => tracing::info!(pc_id = %pc.id, "publisher answer applied"),
                    Err(e) => {
                        // The mids on both sides, because "Mid in answer is not in offer"
                        // names one mid and neither list, which leaves the actual question
                        // -- which m-lines did we offer -- unanswerable from the log.
                        tracing::error!(
                            pc_id = %pc.id,
                            reason = %e,
                            answer_mids = %mid_list(&desc.sdp),
                            "publisher answer failed"
                        );
                    }
                }
                drop(pc);

                // The offer is no longer outstanding, whether or not its answer applied.
                // Sending the owed offer regardless is deliberate: a publisher stuck with
                // a track it announced and never described is worse than one that tries
                // again, and the failure above is already reported.
                let Ok(mut state) = publish_state.lock() else {
                    tracing::error!("publish state lock poisoned; cannot renegotiate");
                    continue;
                };
                state.offer_in_flight = false;
                if std::mem::take(&mut state.renegotiate_pending) {
                    state.offer_in_flight = true;
                    if let Err(e) = send_publisher_offer(&publisher, &signal_sender, &state) {
                        state.offer_in_flight = false;
                        tracing::error!(reason = %e, "deferred renegotiation failed");
                    }
                }
            }
            signal_response::Message::Offer(offer) => {
                // SFU offer for Subscriber PC (initial join, or renegotiation when a new
                // track is subscribed to mid-call).
                tracing::info!(
                    sdp_len = offer.sdp.len(),
                    "subscriber offer received from SFU"
                );
                let desc = SessionDescription {
                    sdp_type: SdpType::Offer,
                    sdp: offer.sdp,
                };
                let answer = {
                    let mut pc = match subscriber.lock() {
                        Ok(guard) => guard,
                        Err(poisoned) => {
                            tracing::error!(
                                reason = "subscriber_lock_poisoned",
                                "subscriber offer failed: recovering poisoned lock"
                            );
                            poisoned.into_inner()
                        }
                    };
                    // X6 (specs/BACKLOG-2026-08-09-errors.md): unlike the Answer arm below
                    // -- where `Ok(None)` means "this answer matches the offer already in
                    // force, there is nothing to apply" and is not an error -- `desc` here
                    // always has `sdp_type: Offer`. `set_remote_description`'s `Offer` arm
                    // (peer_connection.rs) has exactly one return: `accept_offer` either
                    // errors or produces `Ok(Some(answer))`; there is no path that reaches
                    // `Ok(None)` for an offer. So this is not the outage-shaped bug: an SFU
                    // offer that yields no answer would mean the subscriber PC can never
                    // reply, permanently breaking subscription -- a genuine fault, correctly
                    // kept as an error rather than turned into a silent `Ok`.
                    match crate::peer_connection::set_remote_description(&mut pc, &desc) {
                        Ok(Some(ans)) => ans,
                        Ok(None) => {
                            tracing::error!(
                                reason = "no answer produced",
                                "subscriber offer failed"
                            );
                            continue;
                        }
                        Err(e) => {
                            tracing::error!(reason = %e, "subscriber offer failed");
                            continue;
                        }
                    }
                };

                tracing::info!(sdp_len = answer.sdp.len(), "sending subscriber answer");
                if let Err(e) =
                    signal_sender.send(signal_request::Message::Answer(LkSessionDescription {
                        r#type: "answer".to_string(),
                        sdp: answer.sdp,
                        ..Default::default()
                    }))
                {
                    tracing::error!(reason = %e, "sending subscriber answer failed");
                }
            }
            signal_response::Message::Trickle(trickle) => {
                // ICE candidate from SFU
                let target = trickle.target;
                let candidate_json = &trickle.candidate_init;

                // candidate_init is JSON: {"candidate": "...", "sdpMid": "...", "sdpMLineIndex": 0}
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(candidate_json)
                    && let Some(candidate) = parsed.get("candidate").and_then(|c| c.as_str())
                {
                    let handle = if target == i32::from(SignalTarget::Publisher) {
                        &publisher
                    } else {
                        &subscriber
                    };
                    let Ok(mut pc) = handle.lock() else {
                        continue;
                    };
                    if let Err(e) = crate::peer_connection::add_ice_candidate(&mut pc, candidate) {
                        tracing::debug!("Failed to add ICE candidate: {e}");
                    }
                }
            }
            signal_response::Message::Update(update) => {
                for p in &update.participants {
                    if p.state == i32::from(livekit_protocol::participant_info::State::Active) {
                        send_room_event(
                            &event_tx,
                            RoomEvent::ParticipantJoined {
                                room_id: room_id.clone(),
                                identity: p.identity.clone(),
                                sid: p.sid.clone(),
                                name: p.name.clone(),
                            },
                        );
                    } else if p.state
                        == i32::from(livekit_protocol::participant_info::State::Disconnected)
                    {
                        send_room_event(
                            &event_tx,
                            RoomEvent::ParticipantLeft {
                                room_id: room_id.clone(),
                                identity: p.identity.clone(),
                                sid: p.sid.clone(),
                            },
                        );
                    }
                }
            }
            // The SFU telling us which codecs subscribers are actually taking.
            //
            // This is how a mid-call regression arrives: a room where everyone decoded AV1
            // gains a participant who cannot, the SFU works out that the AV1 stream now has
            // no audience, and says so here. Acting on it is the difference between that
            // participant seeing video and seeing nothing, because the SFU does not
            // transcode.
            signal_response::Message::SubscribedQualityUpdate(update) => {
                let wanted: Vec<elementium_codec::VideoCodec> = update
                    .subscribed_codecs
                    .iter()
                    .filter(|c| c.qualities.iter().any(|q| q.enabled))
                    .filter_map(|c| elementium_codec::VideoCodec::from_mime(&c.codec))
                    .collect();

                if wanted.is_empty() {
                    tracing::debug!(
                        track_sid = %update.track_sid,
                        "subscriber quality update named no enabled codec"
                    );
                } else {
                    tracing::info!(
                        track_sid = %update.track_sid,
                        codecs = ?wanted.iter().map(|c| c.sdp_name()).collect::<Vec<_>>(),
                        "SFU reported the codecs subscribers are taking"
                    );
                    send_room_event(
                        &event_tx,
                        RoomEvent::PublishCodecChanged {
                            room_id: room_id.clone(),
                            encrypted,
                            codec: wanted
                                .iter()
                                .min_by_key(|c| c.negotiation_rank())
                                .map_or("VP8", |c| c.sdp_name())
                                .to_string(),
                        },
                    );
                }
            }
            signal_response::Message::TrackPublished(track_published) => {
                tracing::info!(
                    room_id = %room_id,
                    track = ?track_published.track,
                    "Track published confirmed by SFU"
                );
                // Matched back to our key by the cid we announced, which is the only thing
                // the request and this response have in common.
                if let Some(info) = track_published.track.as_ref()
                    && let Ok(mut state) = publish_state.lock()
                {
                    let key = state
                        .published
                        .iter()
                        .find(|(_, cid)| *cid == track_published.cid)
                        .map(|(k, _)| *k);
                    if let Some(key) = key {
                        state.sids.insert(key, info.sid.clone());
                    } else {
                        tracing::warn!(
                            cid = %track_published.cid,
                            "the SFU confirmed a track whose cid we do not recognise; \
                             muting it later will not be possible"
                        );
                    }
                }
            }
            signal_response::Message::SpeakersChanged(speakers) => {
                let identities: Vec<String> = speakers
                    .speakers
                    .iter()
                    .filter(|s| s.active)
                    .map(|s| s.sid.clone())
                    .collect();
                send_room_event(
                    &event_tx,
                    RoomEvent::ActiveSpeakersChanged {
                        room_id: room_id.clone(),
                        speakers: identities,
                    },
                );
            }
            signal_response::Message::Leave(leave) => {
                tracing::info!(
                    room_id = %room_id,
                    reason = leave.reason,
                    "server requested leave"
                );
                send_room_event(
                    &event_tx,
                    RoomEvent::ConnectionStateChanged {
                        room_id: room_id.clone(),
                        state: "disconnected".to_string(),
                    },
                );
                break;
            }
            _ => {
                tracing::debug!(room_id = %room_id, "Unhandled signal message");
            }
        }
    }
    tracing::info!(room_id = %room_id, "Signal processing loop ended");
}

/// Decode one inbound subscriber Opus packet and play it. Returns the updated
/// decoded-frame counter.
///
/// `contiguous` (str0m's gap signal) is intentionally not acted on here -- see the
/// disabled-concealment comment in `audio_pipeline::start_playback` for why: a test
/// (`interleaved_plc_calls_corrupt_subsequent_real_decodes`, elementium-codec/
/// `opus_codec.rs`) proves `conceal()` corrupts the *next real packet's* decode, not just the
/// concealment frame itself, and str0m's own docs say `contiguous` "most likely doesn't
/// matter" for audio -- no confirmed-reliable trigger exists for this yet.
fn decode_and_play_subscriber_audio(
    mid: &str,
    opus_data: &elementium_types::PlaintextMedia,
    opus_decoders: &mut HashMap<String, elementium_codec::OpusDecoder>,
    player: Option<&elementium_media::audio_playback::AudioSink>,
    mut decoded_audio_count: u64,
) -> u64 {
    // Opus decoder creation with fixed, known-valid parameters (48kHz, stereo) cannot
    // fail in practice; the entry API requires an infallible closure.
    let decoder = match opus_decoders.entry(mid.to_string()) {
        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
        std::collections::hash_map::Entry::Vacant(v) => {
            #[allow(clippy::unwrap_used)]
            let d = elementium_codec::OpusDecoder::new(48000, 2).unwrap();
            v.insert(d)
        }
    };

    match decoder.decode(opus_data, 960) {
        Ok(frame) => {
            decoded_audio_count = decoded_audio_count.saturating_add(1);
            // Per-frame, at TRACE so it costs nothing in production. The INFO line below
            // is throttled to every 100th frame, which is fine for eyeballing a live log
            // but makes it impossible to measure what fraction of a sender's frames
            // actually arrived -- and "most of them vanished" is exactly the failure this
            // pipeline has been unable to distinguish from "the audio sounds bad".
            tracing::trace!(
                mid,
                count = decoded_audio_count,
                opus_len = opus_data.len(),
                decoded_samples = frame.data.len(),
                decoded_channels = frame.channels,
                peak = frame.data.iter().fold(0.0_f32, |a, s| a.max(s.abs())),
                "Inbound audio frame decoded"
            );
            if decoded_audio_count.is_multiple_of(100) {
                tracing::info!(
                    mid,
                    count = decoded_audio_count,
                    opus_len = opus_data.len(),
                    decoded_samples = frame.data.len(),
                    decoded_sample_rate = frame.sample_rate,
                    decoded_channels = frame.channels,
                    player_available = player.is_some(),
                    "Decoded inbound Opus audio frame"
                );
            }
            if let Some(p) = player {
                // Keyed by `mid` so the mixer plays this track's consecutive frames
                // sequentially while summing it with other participants' tracks sharing
                // the same output stream -- see `AudioSink::play`.
                p.play(mid, frame);
            }
        }
        Err(e) => {
            tracing::warn!(
                mid,
                opus_len = opus_data.len(),
                error = %e,
                "Failed to decode inbound Opus frame, dropping"
            );
        }
    }

    decoded_audio_count
}

fn process_transport_events(
    event_rx: &Arc<Mutex<mpsc::Receiver<TransportEvent>>>,
    video_frames: &VideoFrameBuffer,
    room_id: &str,
    event_tx: &mpsc::UnboundedSender<RoomEvent>,
) {
    // One decoder per remote track (`mid`), not one shared decoder for the whole
    // subscriber PC -- LiveKit multiplexes every subscribed remote participant's track
    // onto this single subscriber PeerConnection, so a room with more than one other
    // participant genuinely has multiple concurrent audio/video streams here. Opus/VP8
    // decoding is stateful; interleaving two independent encoder streams through one
    // shared decoder corrupts both (this was a real "digital screeching" bug on the
    // native WebRTC path -- see `audio_pipeline.rs` -- and the same risk existed here
    // under an earlier, incorrect "only ever one subscriber stream" assumption).
    let mut opus_decoders: HashMap<String, elementium_codec::OpusDecoder> = HashMap::new();
    // Keyed by mid *and* codec: two remote participants on one subscriber connection can
    // publish different codecs, and a decoder built for the wrong one silently decodes
    // nothing.
    let mut video_decoders: HashMap<
        (String, elementium_codec::VideoCodec),
        elementium_codec::NegotiatedDecoder,
    > = HashMap::new();
    // Process-wide shared output stream, not a stream of its own -- see
    // `elementium_media::audio_playback::shared_sink` for why: this room's subscriber PC
    // is exactly the kind of second concurrent audio source (alongside the native WebRTC
    // path's PeerConnections) that caused a real, confirmed audio-glitching bug when each
    // source opened its own independent cpal output stream.
    let player = elementium_media::audio_playback::shared_sink();
    // room_id never changes for this thread's lifetime, so the frame-buffer key is built
    // once here instead of reallocating a String on every decoded video frame.
    let video_track_key = format!("{room_id}-sub-video");
    tracing::info!(
        player_started = player.is_some(),
        "Subscriber audio playback pipeline initialized"
    );
    let mut decoded_audio_count: u64 = 0;

    loop {
        let event = {
            let Ok(mut rx) = event_rx.lock() else {
                return;
            };
            match rx.try_recv() {
                Ok(event) => Some(event),
                Err(mpsc::error::TryRecvError::Empty) => None,
                // The transport is gone, so nothing will ever arrive here again. Ending
                // the thread matters beyond tidiness: this runs on `spawn_blocking`, and
                // a blocking task that never returns is one the tokio runtime can never
                // shut down. `try_recv().ok()` used to fold this case into `Empty`, so the
                // loop polled a dead channel at 200Hz for the life of the process.
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    tracing::info!("subscriber transport closed; ending media processing");
                    return;
                }
            }
        };

        match event {
            Some(TransportEvent::SubscriberEvent(PcEvent::AudioData {
                mid,
                data: opus_data,
                contiguous: _,
            })) => {
                decoded_audio_count = decode_and_play_subscriber_audio(
                    &mid,
                    &opus_data,
                    &mut opus_decoders,
                    player.as_ref(),
                    decoded_audio_count,
                );
            }
            Some(TransportEvent::SubscriberEvent(PcEvent::VideoData {
                mid,
                data: video_data,
                codec,
            })) => {
                let decoder = match video_decoders.entry((mid.clone(), codec)) {
                    std::collections::hash_map::Entry::Occupied(e) => Some(e.into_mut()),
                    std::collections::hash_map::Entry::Vacant(v) => {
                        elementium_codec::NegotiatedDecoder::new(codec)
                            .ok()
                            .map(|d| v.insert(d))
                    }
                };
                if let Some(decoder) = decoder
                    && let Ok(frames) =
                        elementium_codec::VideoDecoder::decode(decoder, &video_data)
                {
                    for i420_frame in frames {
                        let rgba = elementium_codec::i420_to_rgba(&i420_frame);
                        // Only the first frame's insert needs to clone `video_track_key`;
                        // every later frame overwrites the existing entry in place.
                        if let Ok(mut buf) = video_frames.lock() {
                            if let Some(existing) = buf.get_mut(&video_track_key) {
                                *existing = rgba;
                            } else {
                                buf.insert(video_track_key.clone(), rgba);
                            }
                        }
                    }
                }
            }
            Some(TransportEvent::SubscriberEvent(PcEvent::RemoteTrackAdded { mid, kind })) => {
                // Both identifiers here are honest about being unresolved, which they were
                // not before: `participant_sid` was the literal string "unknown" and
                // `track_sid` was the m-line id, which is ours and not the SFU's track sid
                // at all. A consumer keying off either got a plausible-looking value that
                // had never been measured -- the failure this codebase keeps producing.
                //
                // Resolving them properly means correlating the mid against the
                // `TrackPublished` and participant `Update` messages the signal loop
                // already receives; until something needs it, saying "not resolved" beats
                // inventing an answer.
                tracing::debug!(
                    %mid,
                    ?kind,
                    "remote track subscribed; participant and track sid are not resolved \
                     from the mid yet"
                );
                send_room_event(
                    event_tx,
                    RoomEvent::TrackSubscribed {
                        room_id: room_id.to_string(),
                        participant_sid: String::new(),
                        track_sid: String::new(),
                        mid,
                        kind,
                    },
                );
            }
            Some(TransportEvent::PublisherEvent(PcEvent::IceCandidate(candidate))) => {
                tracing::debug!("Publisher ICE candidate (local): {candidate}");
            }
            Some(TransportEvent::SubscriberEvent(PcEvent::IceCandidate(candidate))) => {
                tracing::debug!("Subscriber ICE candidate (local): {candidate}");
            }
            Some(_) => {}
            None => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
}

fn generate_room_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // System clock is expected to be valid (post-epoch); a failure here would
    // indicate a broken host clock, not a recoverable runtime condition.
    #[allow(clippy::unwrap_used)]
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("lk-{t:x}")
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod media_forwarder_tests {
    use super::{
        EncryptionPolicy, IoCommand, TransportCommand, declared_encryption, encryption,
        spawn_media_forwarder,
    };
    use elementium_e2ee::{E2eeContext, E2eeOptions};
    use elementium_types::{MediaTrackKey, PlaintextMedia};
    use tokio::sync::mpsc;

    /// Encoded media handed to the room must reach the transport unchanged, and as the
    /// matching command variant.
    ///
    /// The bug this guards against was not a corrupted frame but an absent one: the
    /// capture pipelines wrote into a channel nobody was reading, for the whole of every
    /// SFU call. Nothing failed, so nothing was logged.
    #[tokio::test]
    async fn audio_and_video_reach_the_transport_as_matching_commands() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<TransportCommand>(8);
        let media_tx = spawn_media_forwarder(cmd_tx);

        media_tx
            .send(IoCommand::WriteAudio(
                MediaTrackKey::microphone(),
                PlaintextMedia::from_encoder(vec![1, 2, 3]),
            ))
            .await
            .expect("forwarder is running");
        media_tx
            .send(IoCommand::WriteVideo(
                MediaTrackKey::camera(),
                PlaintextMedia::from_encoder(vec![4, 5]),
                elementium_codec::VideoCodec::Vp8,
            ))
            .await
            .expect("forwarder is running");

        match cmd_rx.recv().await {
            Some(TransportCommand::WriteAudio(_, data)) => assert_eq!(data.as_bytes(), &[1, 2, 3]),
            other => panic!("expected WriteAudio, got {}", describe(other.as_ref())),
        }
        match cmd_rx.recv().await {
            Some(TransportCommand::WriteVideo(_, data, _)) => assert_eq!(data.as_bytes(), &[4, 5]),
            other => panic!("expected WriteVideo, got {}", describe(other.as_ref())),
        }
    }

    /// A full transport channel must not block the capture thread: the frame is dropped
    /// and the forwarder stays live for the next one. A microphone cannot be paused.
    #[tokio::test]
    async fn a_full_transport_channel_drops_frames_rather_than_stalling() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<TransportCommand>(1);
        let media_tx = spawn_media_forwarder(cmd_tx);

        for _ in 0..8_u8 {
            media_tx
                .send(IoCommand::WriteAudio(
                    MediaTrackKey::microphone(),
                    PlaintextMedia::from_encoder(vec![7]),
                ))
                .await
                .expect("forwarder stays live while the transport is congested");
        }

        assert!(
            matches!(cmd_rx.recv().await, Some(TransportCommand::WriteAudio(..))),
            "the frames that did fit must still arrive"
        );
    }

    /// The declaration must follow the policy, not a constant.
    ///
    /// Announcing `NONE` while encrypting is the failure this exists to prevent: the SFU
    /// relays the track faithfully and the subscriber, told the payload is plaintext, feeds
    /// ciphertext to its decoder. Nothing errors -- there is simply no audio and no picture.
    #[test]
    fn encryption_is_declared_to_match_what_is_done_to_the_frames() {
        assert_eq!(
            declared_encryption(&EncryptionPolicy::ExplicitlyUnencrypted),
            encryption::Type::None,
        );
        assert_eq!(
            declared_encryption(&EncryptionPolicy::Encrypted(E2eeContext::new(
                E2eeOptions::default()
            ))),
            encryption::Type::Gcm,
            "frames leave encrypted; a subscriber told otherwise cannot decode them",
        );
    }

    fn describe(cmd: Option<&TransportCommand>) -> &'static str {
        match cmd {
            Some(TransportCommand::WriteAudio(..)) => "WriteAudio",
            Some(TransportCommand::WriteVideo(..)) => "WriteVideo",
            Some(TransportCommand::Shutdown) => "Shutdown",
            None => "channel closed",
        }
    }
}
