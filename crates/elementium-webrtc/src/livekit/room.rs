//! LiveKit room state management.
//!
//! Manages the connection lifecycle, participant state, track publishing/subscribing,
//! and bridges signaling messages to the dual PeerConnection transport.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use livekit_protocol::signal_request;
use livekit_protocol::signal_response;
use livekit_protocol::{
    AddTrackRequest, JoinResponse, ParticipantInfo,
    SessionDescription as LkSessionDescription, SignalTarget, TrackInfo, TrackSource, TrackType,
};

use elementium_types::{SdpType, SessionDescription};

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

/// The LiveKit room manages signaling, transport, and participant state.
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
    room_event_tx: mpsc::UnboundedSender<RoomEvent>,
    video_frames: VideoFrameBuffer,
    shutdown: bool,
}

impl LiveKitRoom {
    /// Connect to a LiveKit SFU room.
    ///
    /// 1. Opens WebSocket signaling connection
    /// 2. Waits for JoinResponse
    /// 3. Creates dual PeerConnection transport
    /// 4. Starts the signal processing loop
    ///
    /// Returns the room and a receiver for room events.
    pub async fn connect(
        sfu_url: &str,
        token: &str,
        video_frames: VideoFrameBuffer,
    ) -> Result<(Self, mpsc::UnboundedReceiver<RoomEvent>), String> {
        let room_id = generate_room_id();

        // Connect signaling
        let mut signal_client = SignalClient::connect(sfu_url, token)
            .await
            .map_err(|e| format!("Signaling connect failed: {e}"))?;

        let signal_sender = signal_client.sender();
        let mut signal_rx = signal_client
            .take_receiver()
            .ok_or("Failed to take signal receiver")?;

        // Wait for JoinResponse
        let join_response = wait_for_join(&mut signal_rx).await?;

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
            "Joined LiveKit room"
        );

        // Build initial participant list
        let mut participants = HashMap::new();
        for p in &join_response.other_participants {
            participants.insert(p.sid.clone(), p.clone());
        }

        // Create transport (Publisher + Subscriber PeerConnections)
        let transport = Transport::new(&room_id)?;

        // Room event channel
        let (room_event_tx, room_event_rx) = mpsc::unbounded_channel();

        // Emit initial participants
        for p in participants.values() {
            let _ = room_event_tx.send(RoomEvent::ParticipantJoined {
                room_id: room_id.clone(),
                identity: p.identity.clone(),
                sid: p.sid.clone(),
                name: p.name.clone(),
            });
        }

        let _ = room_event_tx.send(RoomEvent::ConnectionStateChanged {
            room_id: room_id.clone(),
            state: "connected".to_string(),
        });

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
            room_event_tx: room_event_tx.clone(),
            video_frames,
            shutdown: false,
        };

        // Spawn signal processing loop
        let sig_sender = signal_sender;
        let transport_pub = room.transport.publisher.clone();
        let transport_sub = room.transport.subscriber.clone();
        let transport_event_rx = room.transport.event_rx.clone();
        let vf = room.video_frames.clone();
        let rid = room_id.clone();
        let evt_tx = room_event_tx;

        tokio::spawn(async move {
            signal_processing_loop(
                signal_rx,
                sig_sender,
                transport_pub,
                transport_sub,
                transport_event_rx,
                vf,
                rid,
                evt_tx,
            )
            .await;
        });

        Ok((room, room_event_rx))
    }

    /// Publish a local track (audio or video) to the SFU.
    ///
    /// Sends an AddTrackRequest to the SFU, then triggers an SDP renegotiation
    /// on the Publisher PeerConnection.
    pub fn publish_track(
        &mut self,
        kind: &str,
        source: &str,
    ) -> Result<(), String> {
        let track_type: i32 = match kind {
            "audio" => TrackType::Audio.into(),
            "video" => TrackType::Video.into(),
            _ => return Err(format!("Unknown track kind: {kind}")),
        };

        let track_source: i32 = match source {
            "microphone" => TrackSource::Microphone.into(),
            "camera" => TrackSource::Camera.into(),
            "screen_share" => TrackSource::ScreenShare.into(),
            "screen_share_audio" => TrackSource::ScreenShareAudio.into(),
            _ => TrackSource::Unknown.into(),
        };

        let cid = format!("{}-{kind}-{source}", self.local_sid);
        let is_video = kind == "video";

        tracing::info!(
            room_id = %self.room_id,
            kind = kind,
            source = source,
            "Publishing track"
        );

        // Send AddTrack request
        #[allow(deprecated)]
        self.signal_sender
            .send(signal_request::Message::AddTrack(AddTrackRequest {
                cid: cid.clone(),
                name: format!("{kind}_{source}"),
                r#type: track_type,
                source: track_source,
                width: if is_video { 640 } else { 0 },
                height: if is_video { 480 } else { 0 },
                ..Default::default()
            }))
            .map_err(|e| format!("Failed to send AddTrack: {e}"))?;

        // Trigger SDP renegotiation on Publisher
        let include_video = is_video || self.has_video_track();
        let offer = self.transport.create_publisher_offer(include_video)?;

        self.signal_sender
            .send(signal_request::Message::Offer(LkSessionDescription {
                r#type: "offer".to_string(),
                sdp: offer.sdp,
                ..Default::default()
            }))
            .map_err(|e| format!("Failed to send publisher offer: {e}"))?;

        Ok(())
    }

    /// Check if we already have a published video track.
    fn has_video_track(&self) -> bool {
        let video_type: i32 = TrackType::Video.into();
        self.local_tracks.iter().any(|t| t.r#type == video_type)
    }

    /// Send a media command to the transport (audio/video data).
    pub async fn write_audio(&self, data: Vec<u8>) -> Result<(), String> {
        self.transport
            .send_command(TransportCommand::WriteAudio(data))
            .await
    }

    pub async fn write_video(&self, data: Vec<u8>) -> Result<(), String> {
        self.transport
            .send_command(TransportCommand::WriteVideo(data))
            .await
    }

    /// Get the list of current participants.
    pub fn participants(&self) -> Vec<&ParticipantInfo> {
        self.participants.values().collect()
    }

    /// Disconnect from the room.
    pub async fn disconnect(&mut self) {
        if self.shutdown {
            return;
        }
        self.shutdown = true;

        tracing::info!(room_id = %self.room_id, "Disconnecting from LiveKit room");

        let _ = self.room_event_tx.send(RoomEvent::ConnectionStateChanged {
            room_id: self.room_id.clone(),
            state: "disconnected".to_string(),
        });

        self.transport.shutdown().await;
        self.signal_client.disconnect().await;
    }
}

/// Wait for the JoinResponse from the signaling channel.
async fn wait_for_join(
    rx: &mut mpsc::UnboundedReceiver<signal_response::Message>,
) -> Result<JoinResponse, String> {
    let timeout = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(msg) = rx.recv().await {
            if let signal_response::Message::Join(join) = msg {
                return Ok(join);
            }
            tracing::debug!("Ignoring pre-join message");
        }
        Err("Signal channel closed before JoinResponse".to_string())
    });

    match timeout.await {
        Ok(result) => result,
        Err(_) => Err("Timeout waiting for JoinResponse".to_string()),
    }
}

/// Background loop: processes signaling messages and transport events.
#[allow(clippy::too_many_arguments)]
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
    // Must be blocking because AudioPlayer (cpal) is not Send.
    let vf = video_frames;
    let rid = room_id.clone();
    let evt = event_tx.clone();
    tokio::task::spawn_blocking(move || {
        process_transport_events(transport_event_rx, vf, rid, evt);
    });

    // Process signaling messages
    while let Some(msg) = signal_rx.recv().await {
        match msg {
            signal_response::Message::Answer(answer) => {
                // SFU answer for our Publisher offer
                let desc = SessionDescription {
                    sdp_type: SdpType::Answer,
                    sdp: answer.sdp,
                };
                let mut pc = match publisher.lock() {
                    Ok(pc) => pc,
                    Err(_) => continue,
                };
                if let Err(e) = crate::peer_connection::set_remote_description(&mut pc, &desc) {
                    tracing::error!("Failed to set publisher answer: {e}");
                }
            }
            signal_response::Message::Offer(offer) => {
                // SFU offer for Subscriber PC
                let desc = SessionDescription {
                    sdp_type: SdpType::Offer,
                    sdp: offer.sdp,
                };
                let answer = {
                    let mut pc = match subscriber.lock() {
                        Ok(pc) => pc,
                        Err(_) => continue,
                    };
                    match crate::peer_connection::set_remote_description(&mut pc, &desc) {
                        Ok(Some(ans)) => ans,
                        Ok(None) => {
                            tracing::error!("Expected answer from subscriber offer");
                            continue;
                        }
                        Err(e) => {
                            tracing::error!("Failed to set subscriber offer: {e}");
                            continue;
                        }
                    }
                };

                if let Err(e) =
                    signal_sender.send(signal_request::Message::Answer(LkSessionDescription {
                        r#type: "answer".to_string(),
                        sdp: answer.sdp,
                        ..Default::default()
                    }))
                {
                    tracing::error!("Failed to send subscriber answer: {e}");
                }
            }
            signal_response::Message::Trickle(trickle) => {
                // ICE candidate from SFU
                let target = trickle.target;
                let candidate_json = &trickle.candidate_init;

                // candidate_init is JSON: {"candidate": "...", "sdpMid": "...", "sdpMLineIndex": 0}
                if let Ok(parsed) =
                    serde_json::from_str::<serde_json::Value>(candidate_json)
                {
                    if let Some(candidate) = parsed.get("candidate").and_then(|c| c.as_str()) {
                        let handle = if target == SignalTarget::Publisher as i32 {
                            &publisher
                        } else {
                            &subscriber
                        };
                        let mut pc = match handle.lock() {
                            Ok(pc) => pc,
                            Err(_) => continue,
                        };
                        if let Err(e) =
                            crate::peer_connection::add_ice_candidate(&mut pc, candidate)
                        {
                            tracing::debug!("Failed to add ICE candidate: {e}");
                        }
                    }
                }
            }
            signal_response::Message::Update(update) => {
                for p in &update.participants {
                    if p.state == livekit_protocol::participant_info::State::Active as i32 {
                        let _ = event_tx.send(RoomEvent::ParticipantJoined {
                            room_id: room_id.clone(),
                            identity: p.identity.clone(),
                            sid: p.sid.clone(),
                            name: p.name.clone(),
                        });
                    } else if p.state
                        == livekit_protocol::participant_info::State::Disconnected as i32
                    {
                        let _ = event_tx.send(RoomEvent::ParticipantLeft {
                            room_id: room_id.clone(),
                            identity: p.identity.clone(),
                            sid: p.sid.clone(),
                        });
                    }
                }
            }
            signal_response::Message::TrackPublished(published) => {
                tracing::info!(
                    room_id = %room_id,
                    track = ?published.track,
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
                let _ = event_tx.send(RoomEvent::ActiveSpeakersChanged {
                    room_id: room_id.clone(),
                    speakers: identities,
                });
            }
            signal_response::Message::Leave(leave) => {
                tracing::info!(
                    room_id = %room_id,
                    reason = leave.reason,
                    "Server requested leave"
                );
                let _ = event_tx.send(RoomEvent::ConnectionStateChanged {
                    room_id: room_id.clone(),
                    state: "disconnected".to_string(),
                });
                break;
            }
            _ => {
                tracing::debug!(room_id = %room_id, "Unhandled signal message");
            }
        }
    }
    tracing::info!(room_id = %room_id, "Signal processing loop ended");
}

/// Process transport events (audio/video data from subscriber PC).
///
/// Runs as a blocking task because `AudioPlayer` (cpal) is not `Send`.
fn process_transport_events(
    event_rx: Arc<Mutex<mpsc::Receiver<TransportEvent>>>,
    video_frames: VideoFrameBuffer,
    room_id: String,
    event_tx: mpsc::UnboundedSender<RoomEvent>,
) {
    let mut opus_decoders: HashMap<String, elementium_codec::OpusDecoder> = HashMap::new();
    let mut vp8_decoder = elementium_codec::Vp8Decoder::new().ok();
    let player = elementium_media::audio_playback::AudioPlayer::start().ok();

    loop {
        let event = {
            let mut rx = match event_rx.lock() {
                Ok(rx) => rx,
                Err(_) => return,
            };
            rx.try_recv().ok()
        };

        match event {
            Some(TransportEvent::SubscriberEvent(PcEvent::AudioData(opus_data))) => {
                let decoder = opus_decoders
                    .entry("default".to_string())
                    .or_insert_with(|| {
                        elementium_codec::OpusDecoder::new(48000, 2).unwrap()
                    });

                if let Ok(frame) = decoder.decode(&opus_data, 960) {
                    if let Some(ref p) = player {
                        p.play(frame);
                    }
                }
            }
            Some(TransportEvent::SubscriberEvent(PcEvent::VideoData(vp8_data))) => {
                if let Some(ref mut decoder) = vp8_decoder {
                    if let Ok(frames) = decoder.decode(&vp8_data) {
                        for i420_frame in frames {
                            let rgba = elementium_codec::i420_to_rgba(&i420_frame);
                            let track_key = format!("{room_id}-sub-video");
                            if let Ok(mut buf) = video_frames.lock() {
                                buf.insert(track_key, rgba);
                            }
                        }
                    }
                }
            }
            Some(TransportEvent::SubscriberEvent(PcEvent::RemoteTrackAdded { mid, kind })) => {
                tracing::info!(
                    room_id = %room_id,
                    mid = %mid,
                    kind = %kind,
                    "Remote track added from subscriber PC"
                );
                let _ = event_tx.send(RoomEvent::TrackSubscribed {
                    room_id: room_id.clone(),
                    participant_sid: "unknown".to_string(),
                    track_sid: mid,
                    kind,
                });
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
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("lk-{t:x}")
}
