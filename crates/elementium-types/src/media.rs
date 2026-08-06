use serde::{Deserialize, Serialize};

/// Encoded media bytes in the clear: codec-readable, not safe to put on the wire.
///
/// One half of a pair with [`WireMedia`] that makes the encryption boundary explicit in
/// the type system, in both directions:
///
/// ```text
/// inbound:   WireMedia --decrypt--> PlaintextMedia --> decoder
/// outbound:  encoder --> PlaintextMedia --encrypt--> WireMedia --> network
/// ```
///
/// This exists because of a real, expensive bug: with both directions typed as plain
/// `Vec<u8>`, still-encrypted inbound bytes were handed straight to the Opus decoder when
/// no E2EE key had been configured. libopus does not reject corrupt input -- it decodes
/// ciphertext to *noise* -- so the failure produced screeching audio with a completely
/// clean log: no decode errors, correct packet pacing, plausible amplitudes. Six separate
/// investigations searched the output stage before the input was ever questioned.
///
/// Every constructor is named for the provenance it asserts, so the compiler forces each
/// call site to state which side of the boundary it is on, and any claim that bytes are
/// safe to decode is greppable rather than implicit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaintextMedia(Vec<u8>);

impl PlaintextMedia {
    /// Bytes that just came out of a successful authenticated decryption.
    #[must_use]
    pub const fn from_decrypted(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Bytes that just came out of a local encoder, before any encryption.
    #[must_use]
    pub const fn from_encoder(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Accept wire bytes as plaintext because the remote peer is known not to be
    /// encrypting.
    ///
    /// Deliberately verbose: this is the exact hole the type pair exists to close. If the
    /// peer *is* encrypting after all, this hands ciphertext to a decoder, which produces
    /// noise rather than an error. Callers using this must independently validate that
    /// the payload really is decodable (e.g. by parsing the codec's own framing) rather
    /// than assuming a clean decode means clean input.
    #[must_use]
    pub fn assume_peer_sends_unencrypted(wire: WireMedia) -> Self {
        Self(wire.into_bytes())
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Consume into raw bytes, for handing to an encryption routine.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

/// Encoded media bytes as they travel over RTP: possibly encrypted, never codec-readable.
///
/// The other half of the pair described on [`PlaintextMedia`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireMedia(Vec<u8>);

impl WireMedia {
    /// Bytes received from the network, provenance unknown -- may be ciphertext.
    #[must_use]
    pub const fn from_network(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Bytes produced by a successful encryption, ready to send.
    #[must_use]
    pub const fn from_encrypted(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Send plaintext on the wire because this connection deliberately has no E2EE.
    ///
    /// Named to match [`crate::media::PlaintextMedia::assume_peer_sends_unencrypted`]'s
    /// intent: an unencrypted send must be a stated choice, never something that happens
    /// because a key was forgotten.
    #[must_use]
    pub fn deliberately_unencrypted(plaintext: PlaintextMedia) -> Self {
        Self(plaintext.into_bytes())
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Consume into raw bytes, for handing to a decryption routine or the socket.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

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

#[cfg(test)]
mod media_boundary_tests {
    use super::*;

    /// Bytes must survive the round trip unchanged -- the types are a compile-time
    /// provenance marker, not a transformation.
    #[test]
    fn wrapping_preserves_bytes_exactly() {
        let raw = vec![1u8, 2, 3, 4, 5];
        assert_eq!(PlaintextMedia::from_decrypted(raw.clone()).into_bytes(), raw);
        assert_eq!(WireMedia::from_network(raw.clone()).into_bytes(), raw);
        assert_eq!(PlaintextMedia::from_encoder(raw.clone()).as_bytes(), raw.as_slice());
        assert_eq!(WireMedia::from_encrypted(raw.clone()).as_bytes(), raw.as_slice());
    }

    /// Crossing the boundary in either direction is byte-preserving but requires naming
    /// the assertion being made, which is the entire point of the pair.
    #[test]
    fn explicit_boundary_crossings_preserve_bytes() {
        let raw = vec![9u8, 8, 7];
        let wire = WireMedia::from_network(raw.clone());
        assert_eq!(PlaintextMedia::assume_peer_sends_unencrypted(wire).as_bytes(), raw.as_slice());

        let plain = PlaintextMedia::from_encoder(raw.clone());
        assert_eq!(WireMedia::deliberately_unencrypted(plain).as_bytes(), raw.as_slice());
    }

    /// Length/emptiness accessors report the payload, not the wrapper.
    #[test]
    fn length_reflects_payload() {
        assert!(PlaintextMedia::from_encoder(Vec::new()).is_empty());
        assert_eq!(WireMedia::from_network(vec![0; 42]).len(), 42);
    }
}
