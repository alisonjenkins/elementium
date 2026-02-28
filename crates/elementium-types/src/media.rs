use serde::{Deserialize, Serialize};

/// Raw video frame in RGBA format, ready for display.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    /// RGBA pixel data, length = width * height * 4
    pub data: Vec<u8>,
    pub timestamp_us: u64,
}

/// Raw audio frame as interleaved f32 samples.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved f32 PCM samples
    pub data: Vec<f32>,
    pub timestamp_us: u64,
}

/// Video frame in I420 (YUV 4:2:0 planar) format, used for encoding.
#[derive(Debug, Clone)]
pub struct I420Frame {
    pub width: u32,
    pub height: u32,
    /// Y plane, length = width * height
    pub y: Vec<u8>,
    /// U plane, length = (width/2) * (height/2)
    pub u: Vec<u8>,
    /// V plane, length = (width/2) * (height/2)
    pub v: Vec<u8>,
    pub timestamp_us: u64,
}

/// A media device (microphone, camera, speaker).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaDevice {
    pub id: String,
    pub label: String,
    pub kind: MediaDeviceKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MediaDeviceKind {
    AudioInput,
    AudioOutput,
    VideoInput,
}

/// Constraints for getUserMedia requests.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MediaConstraints {
    pub audio: Option<AudioConstraints>,
    pub video: Option<VideoConstraints>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConstraints {
    pub device_id: Option<String>,
    pub echo_cancellation: Option<bool>,
    pub noise_suppression: Option<bool>,
    pub auto_gain_control: Option<bool>,
}

impl Default for AudioConstraints {
    fn default() -> Self {
        Self {
            device_id: None,
            echo_cancellation: Some(true),
            noise_suppression: Some(true),
            auto_gain_control: Some(true),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoConstraints {
    pub device_id: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<f64>,
}

impl Default for VideoConstraints {
    fn default() -> Self {
        Self {
            device_id: None,
            width: Some(1280),
            height: Some(720),
            frame_rate: Some(30.0),
        }
    }
}

/// Track identifier used across the IPC boundary.
#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackId(pub String);

impl std::fmt::Display for TrackId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// ICE candidate exchanged during signaling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceCandidate {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    pub sdp_m_line_index: Option<u16>,
}

/// SDP offer or answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDescription {
    #[serde(rename = "type")]
    pub sdp_type: SdpType,
    pub sdp: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SdpType {
    Offer,
    Answer,
}

/// Peer connection state reported to the frontend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerConnectionState {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}

/// ICE connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IceConnectionState {
    New,
    Checking,
    Connected,
    Completed,
    Failed,
    Disconnected,
    Closed,
}

/// Screen capture source (monitor or window).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureSource {
    pub id: String,
    pub name: String,
    pub kind: CaptureSourceKind,
    /// Thumbnail as PNG bytes (optional).
    #[serde(skip)]
    pub thumbnail: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CaptureSourceKind {
    Monitor,
    Window,
}
