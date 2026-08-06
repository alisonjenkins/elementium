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
    AddTrackRequest, JoinResponse, ParticipantInfo,
    SessionDescription as LkSessionDescription, SignalTarget, TrackInfo, TrackSource, TrackType,
};

use elementium_types::{CorrelationId, SdpType, SessionDescription};

use crate::e2ee_io::EncryptionPolicy;
use crate::engine::VideoFrameBuffer;
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
        participant_sid: String,
        track_sid: String,
        kind: String,
    },
    #[serde(rename_all = "camelCase")]
    TrackUnsubscribed {
        room_id: String,
        participant_sid: String,
        track_sid: String,
    },
    #[serde(rename_all = "camelCase")]
    ConnectionStateChanged {
        room_id: String,
        state: String,
    },
    #[serde(rename_all = "camelCase")]
    ActiveSpeakersChanged {
        room_id: String,
        speakers: Vec<String>,
    },
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
    local_tracks: Vec<TrackInfo>,
    /// `cid` of the published audio/video track, kept so a later renegotiation re-states
    /// the same msid track ids the SFU already associated with these tracks.
    audio_cid: Option<String>,
    video_cid: Option<String>,
    room_event_tx: mpsc::UnboundedSender<RoomEvent>,
    video_frames: VideoFrameBuffer,
    shutdown: bool,
    correlation_id: CorrelationId,
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

        // Connect signaling
        let mut signal_client = match SignalClient::connect(sfu_url, token).await {
            Ok(client) => client,
            Err(e) => {
                tracing::error!(reason = %e, "signaling connect failed");
                return Err(crate::error::WebRtcError::Signaling(format!(
                    "connect failed: {e}"
                )));
            }
        };

        let signal_sender = signal_client.sender();
        let Some(mut signal_rx) = signal_client.take_receiver() else {
            tracing::error!(reason = "signal receiver already taken", "connect attempt failed");
            return Err(crate::error::WebRtcError::ChannelClosed(
                "signal receiver already taken",
            ));
        };

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

        let room = Self {
            room_id: room_id.clone(),
            room_name,
            local_identity,
            local_sid,
            signal_sender: signal_sender.clone(),
            signal_client,
            transport,
            participants,
            local_tracks: Vec::new(),
            audio_cid: None,
            video_cid: None,
            room_event_tx: room_event_tx.clone(),
            video_frames,
            shutdown: false,
            correlation_id,
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
    pub fn publish_track(
        &mut self,
        kind: &str,
        source: &str,
    ) -> Result<(), crate::error::WebRtcError> {
        let track_type: i32 = match kind {
            "audio" => TrackType::Audio.into(),
            "video" => TrackType::Video.into(),
            _ => return Err(crate::error::WebRtcError::UnknownTrackKind(kind.to_string())),
        };

        let track_source: i32 = match source {
            "microphone" => TrackSource::Microphone.into(),
            "camera" => TrackSource::Camera.into(),
            "screen_share" => TrackSource::ScreenShare.into(),
            "screen_share_audio" => TrackSource::ScreenShareAudio.into(),
            _ => TrackSource::Unknown.into(),
        };

        // The SFU pairs this request with an m-line by matching `cid` against the offer's
        // msid track id, so the same value has to reach both. livekit-client gets this for
        // free (its cid *is* the MediaStreamTrack id); we have to thread it through
        // explicitly, or the association falls back to the server's match-by-kind guess and
        // becomes order-dependent.
        let cid = format!("{}-{kind}-{source}", self.local_sid);
        let is_video = kind == "video";

        let span = tracing::info_span!("session", correlation_id = %self.correlation_id, room_id = %self.room_id);
        let _guard = span.enter();

        tracing::info!(kind = kind, source = source, "publishing track");

        // Send AddTrack request
        #[allow(deprecated)]
        if let Err(e) = self
            .signal_sender
            .send(signal_request::Message::AddTrack(AddTrackRequest {
                cid: cid.clone(),
                name: format!("{kind}_{source}"),
                r#type: track_type,
                source: track_source,
                width: if is_video { 640 } else { 0 },
                height: if is_video { 480 } else { 0 },
                ..Default::default()
            }))
        {
            tracing::error!(reason = %e, "publish track failed");
            return Err(crate::error::WebRtcError::Signaling(format!(
                "failed to send AddTrack: {e}"
            )));
        }

        // Trigger SDP renegotiation on Publisher
        let include_video = is_video || self.has_video_track();
        let (audio_cid, video_cid) = if is_video {
            (self.audio_cid.clone(), Some(cid.clone()))
        } else {
            (Some(cid.clone()), self.video_cid.clone())
        };
        if is_video {
            self.video_cid = Some(cid.clone());
        } else {
            self.audio_cid = Some(cid.clone());
        }
        tracing::info!(cid = %cid, kind, source, "AddTrack cid, also used as the offer msid track id");
        let offer = match self.transport.create_publisher_offer(include_video, audio_cid, video_cid) {
            Ok(o) => o,
            Err(e) => {
                tracing::error!(reason = %e, "publish track failed");
                return Err(e);
            }
        };

        if let Err(e) = self
            .signal_sender
            .send(signal_request::Message::Offer(LkSessionDescription {
                r#type: "offer".to_string(),
                sdp: offer.sdp,
                ..Default::default()
            }))
        {
            tracing::error!(reason = %e, "publish track failed");
            return Err(crate::error::WebRtcError::Signaling(format!(
                "failed to send publisher offer: {e}"
            )));
        }

        Ok(())
    }

    /// Check if we already have a published video track.
    fn has_video_track(&self) -> bool {
        let video_type: i32 = TrackType::Video.into();
        self.local_tracks.iter().any(|t| t.r#type == video_type)
    }

    /// Send a media command to the transport (audio/video data).
    ///
    /// # Errors
    ///
    /// Returns `Err` if the command cannot be sent to the transport.
    pub async fn write_audio(
        &self,
        data: elementium_types::PlaintextMedia,
    ) -> Result<(), crate::error::WebRtcError> {
        self.transport
            .send_command(TransportCommand::WriteAudio(data))
            .await
    }

    /// # Errors
    ///
    /// Returns `Err` if the command cannot be sent to the transport.
    pub async fn write_video(
        &self,
        data: elementium_types::PlaintextMedia,
    ) -> Result<(), crate::error::WebRtcError> {
        self.transport
            .send_command(TransportCommand::WriteVideo(data))
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
            kind,
        } => {
            tracing::info!(
                room_id = %room_id,
                participant_sid = %participant_sid,
                track_sid = %track_sid,
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
        RoomEvent::ActiveSpeakersChanged { room_id, speakers } => {
            tracing::debug!(room_id = %room_id, speakers = ?speakers, "active speakers changed");
        }
    }
    let _ = tx.send(event);
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
                tracing::info!(sdp_len = answer.sdp.len(), "publisher answer received from SFU");
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
                    Err(e) => tracing::error!(pc_id = %pc.id, reason = %e, "publisher answer failed"),
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
            signal_response::Message::TrackPublished(track_published) => {
                tracing::info!(
                    room_id = %room_id,
                    track = ?track_published.track,
                    "Track published confirmed by SFU"
                );
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
    let mut vp8_decoders: HashMap<String, elementium_codec::Vp8Decoder> = HashMap::new();
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
            rx.try_recv().ok()
        };

        match event {
            Some(TransportEvent::SubscriberEvent(PcEvent::AudioData { mid, data: opus_data, contiguous: _ })) => {
                decoded_audio_count = decode_and_play_subscriber_audio(
                    &mid,
                    &opus_data,
                    &mut opus_decoders,
                    player.as_ref(),
                    decoded_audio_count,
                );
            }
            Some(TransportEvent::SubscriberEvent(PcEvent::VideoData { mid, data: vp8_data })) => {
                let decoder = match vp8_decoders.entry(mid.clone()) {
                    std::collections::hash_map::Entry::Occupied(e) => Some(e.into_mut()),
                    std::collections::hash_map::Entry::Vacant(v) => {
                        elementium_codec::Vp8Decoder::new().ok().map(|d| v.insert(d))
                    }
                };
                if let Some(decoder) = decoder
                    && let Ok(frames) = decoder.decode(&vp8_data)
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
                send_room_event(
                    event_tx,
                    RoomEvent::TrackSubscribed {
                        room_id: room_id.to_string(),
                        participant_sid: "unknown".to_string(),
                        track_sid: mid,
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
