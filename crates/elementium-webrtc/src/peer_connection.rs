use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::format::Codec;
use str0m::media::{Direction, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};

use elementium_types::{
    IceConnectionState as IceState, PeerConnectionState, SdpType, SessionDescription,
};

/// Events emitted by a peer connection to the application layer.
#[derive(Debug, Clone)]
pub enum PcEvent {
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
    /// Received audio data (Opus packet).
    AudioData(Vec<u8>),
    /// Received video data (VP8 packet).
    VideoData(Vec<u8>),
}

/// Shared state for a single peer connection.
pub struct PeerConnectionInner {
    pub id: String,
    pub rtc: Rtc,
    pub audio_mid: Option<Mid>,
    pub video_mid: Option<Mid>,
    pub pending_offer: Option<SdpPendingOffer>,
    pub remote_mids: HashMap<Mid, MediaKind>,
    pub audio_frame_count: u64,
    pub video_frame_count: u64,
    pub alive: bool,
}

/// Thread-safe handle to a peer connection.
pub type PeerConnectionHandle = Arc<Mutex<PeerConnectionInner>>;

/// Create a new peer connection configured for audio and video.
pub fn create_peer_connection(id: String) -> PeerConnectionInner {
    let rtc = RtcConfig::new()
        .clear_codecs()
        .enable_opus(true)
        .enable_vp8(true)
        .build(Instant::now());

    PeerConnectionInner {
        id,
        rtc,
        audio_mid: None,
        video_mid: None,
        pending_offer: None,
        remote_mids: HashMap::new(),
        audio_frame_count: 0,
        video_frame_count: 0,
        alive: true,
    }
}

/// Add a local UDP candidate to the peer connection.
pub fn add_local_candidate(pc: &mut PeerConnectionInner, addr: SocketAddr) {
    if let Ok(candidate) = Candidate::host(addr, "udp") {
        let _ = pc.rtc.add_local_candidate(candidate);
    }
}

/// Create an SDP offer with audio and optionally video tracks.
pub fn create_offer(
    pc: &mut PeerConnectionInner,
    include_video: bool,
) -> Result<SessionDescription, String> {
    let mut api = pc.rtc.sdp_api();

    let audio_mid = api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    pc.audio_mid = Some(audio_mid);

    if include_video {
        let video_mid = api.add_media(MediaKind::Video, Direction::SendRecv, None, None, None);
        pc.video_mid = Some(video_mid);
    }

    let (offer, pending) = api.apply().ok_or("No changes to apply")?;
    pc.pending_offer = Some(pending);

    Ok(SessionDescription {
        sdp_type: SdpType::Offer,
        sdp: offer.to_sdp_string(),
    })
}

/// Create an SDP answer (after receiving a remote offer).
pub fn create_answer(_pc: &mut PeerConnectionInner) -> Result<SessionDescription, String> {
    // The answer is already generated during set_remote_description for offers.
    Err("Answer is generated during set_remote_description".into())
}

/// Set the remote description (offer or answer).
pub fn set_remote_description(
    pc: &mut PeerConnectionInner,
    desc: &SessionDescription,
) -> Result<Option<SessionDescription>, String> {
    match desc.sdp_type {
        SdpType::Offer => {
            let offer = SdpOffer::from_sdp_string(&desc.sdp)
                .map_err(|e| format!("Invalid offer SDP: {e}"))?;
            let answer = pc
                .rtc
                .sdp_api()
                .accept_offer(offer)
                .map_err(|e| format!("Failed to accept offer: {e}"))?;

            let answer_sdp = answer.to_sdp_string();
            Ok(Some(SessionDescription {
                sdp_type: SdpType::Answer,
                sdp: answer_sdp,
            }))
        }
        SdpType::Answer => {
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
            Ok(None)
        }
    }
}

/// Add a remote ICE candidate.
pub fn add_ice_candidate(
    pc: &mut PeerConnectionInner,
    candidate_sdp: &str,
) -> Result<(), String> {
    if candidate_sdp.is_empty() {
        return Ok(());
    }
    let candidate = Candidate::from_sdp_string(candidate_sdp)
        .map_err(|e| format!("Invalid ICE candidate: {e}"))?;
    pc.rtc.add_remote_candidate(candidate);
    Ok(())
}

/// Write an Opus-encoded audio frame to the peer connection.
pub fn write_audio(pc: &mut PeerConnectionInner, opus_data: &[u8]) -> Result<(), String> {
    let mid = pc.audio_mid.ok_or("No audio mid configured")?;

    let Some(writer) = pc.rtc.writer(mid) else {
        return Err("No writer for audio mid".into());
    };

    let pt = writer
        .payload_params()
        .find(|p| p.spec().codec == Codec::Opus)
        .map(|p| p.pt())
        .ok_or("No Opus payload type negotiated")?;

    // Opus at 48kHz: each 20ms frame = 960 samples
    let samples_per_frame: u64 = 960;
    let rtp_time = MediaTime::new(
        pc.audio_frame_count * samples_per_frame,
        NonZeroU32::new(48_000).unwrap().into(),
    );

    writer
        .write(pt, Instant::now(), rtp_time, opus_data)
        .map_err(|e| format!("Failed to write audio: {e}"))?;

    pc.audio_frame_count += 1;
    Ok(())
}

/// Write a VP8-encoded video frame to the peer connection.
pub fn write_video(pc: &mut PeerConnectionInner, vp8_data: &[u8]) -> Result<(), String> {
    let mid = pc.video_mid.ok_or("No video mid configured")?;

    let Some(writer) = pc.rtc.writer(mid) else {
        return Err("No writer for video mid".into());
    };

    let pt = writer
        .payload_params()
        .find(|p| p.spec().codec == Codec::Vp8)
        .map(|p| p.pt())
        .ok_or("No VP8 payload type negotiated")?;

    // VP8 at 90kHz clock, 30fps = 3000 ticks per frame
    let ticks_per_frame: u64 = 3000;
    let rtp_time = MediaTime::new(
        pc.video_frame_count * ticks_per_frame,
        NonZeroU32::new(90_000).unwrap().into(),
    );

    writer
        .write(pt, Instant::now(), rtp_time, vp8_data)
        .map_err(|e| format!("Failed to write video: {e}"))?;

    pc.video_frame_count += 1;
    Ok(())
}

/// Run one iteration of the I/O loop. Returns events and next deadline.
pub fn poll_once(
    pc: &mut PeerConnectionInner,
    socket: &UdpSocket,
    _recv_buf: &mut [u8],
) -> Result<(Vec<PcEvent>, Instant), String> {
    let mut events = Vec::new();

    loop {
        match pc.rtc.poll_output() {
            Ok(Output::Transmit(transmit)) => {
                let _ = socket.send_to(&transmit.contents, transmit.destination);
            }
            Ok(Output::Event(event)) => {
                if let Some(pc_event) = handle_str0m_event(pc, event) {
                    events.push(pc_event);
                }
            }
            Ok(Output::Timeout(deadline)) => {
                return Ok((events, deadline));
            }
            Err(e) => {
                pc.alive = false;
                return Err(format!("str0m error: {e}"));
            }
        }
    }
}

/// Try to receive a UDP packet and feed it to str0m.
pub fn recv_and_feed(
    pc: &mut PeerConnectionInner,
    socket: &UdpSocket,
    recv_buf: &mut [u8],
    timeout: Duration,
) -> Result<(), String> {
    socket
        .set_read_timeout(Some(timeout.max(Duration::from_millis(1))))
        .map_err(|e| e.to_string())?;

    match socket.recv_from(recv_buf) {
        Ok((len, source)) => {
            let input = Input::Receive(
                Instant::now(),
                Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: socket.local_addr().map_err(|e| e.to_string())?,
                    contents: recv_buf[..len]
                        .try_into()
                        .map_err(|e| format!("{e:?}"))?,
                },
            );
            pc.rtc
                .handle_input(input)
                .map_err(|e| format!("handle_input error: {e}"))?;
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
            return Err(format!("recv error: {e}"));
        }
    }
    Ok(())
}

fn handle_str0m_event(pc: &mut PeerConnectionInner, event: Event) -> Option<PcEvent> {
    match event {
        Event::IceConnectionStateChange(state) => {
            tracing::info!(pc_id = %pc.id, ?state, "ICE state changed");
            let mapped = match state {
                IceConnectionState::New => IceState::New,
                IceConnectionState::Checking => IceState::Checking,
                IceConnectionState::Connected => IceState::Connected,
                IceConnectionState::Completed => IceState::Completed,
                IceConnectionState::Disconnected => {
                    pc.alive = false;
                    IceState::Disconnected
                }
            };
            Some(PcEvent::IceConnectionStateChange(mapped))
        }
        Event::Connected => {
            tracing::info!(pc_id = %pc.id, "Peer connected (DTLS+ICE)");
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
            Some(PcEvent::RemoteTrackAdded {
                mid: media.mid.to_string(),
                kind: match media.kind {
                    MediaKind::Audio => "audio".to_string(),
                    MediaKind::Video => "video".to_string(),
                },
            })
        }
        Event::MediaData(data) => {
            if data.params.spec().codec == Codec::Opus {
                Some(PcEvent::AudioData(data.data.to_vec()))
            } else if data.params.spec().codec == Codec::Vp8 {
                Some(PcEvent::VideoData(data.data.to_vec()))
            } else {
                None
            }
        }
        _ => {
            tracing::debug!(pc_id = %pc.id, ?event, "WebRTC event");
            None
        }
    }
}
