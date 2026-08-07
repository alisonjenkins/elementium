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
///
/// One allocation holds all three planes, laid out Y then U then V, with each plane's rows
/// padded to its stride. That shape is not an implementation detail to be tidied away: it
/// is what a JPEG decoder and a hardware capture buffer already hand over, so a frame can
/// be adopted from one without copying a megabyte per frame. Requiring tightly-packed
/// planes forced exactly that copy, thirty times a second, for the length of every call.
///
/// The fields are private because a stride that can be ignored will be. Reading a padded
/// plane as though its rows were `width` bytes shears the picture into sliding horizontal
/// bands with rotated colour channels -- a fault this codebase has produced more than once
/// and which looks like a broken camera or a broken encoder rather than a wrong number.
/// The accessors make it unrepresentable.
#[derive(Debug, Clone, Default)]
pub struct I420Frame {
    width: u32,
    height: u32,
    /// Y, U and V planes, consecutively, each row padded to its plane's stride.
    data: Vec<u8>,
    /// Bytes per row of the Y plane. At least `width`.
    y_stride: usize,
    /// Bytes per row of each chroma plane. At least `width.div_ceil(2)`.
    uv_stride: usize,
    timestamp_us: u64,
}

impl I420Frame {
    /// Build a frame from three tightly-packed planes.
    ///
    /// For callers that produce planes themselves -- colour converters, tests, synthetic
    /// sources. A decoder that already has a padded buffer should use
    /// [`I420Frame::from_padded`] instead and avoid the copy entirely.
    ///
    /// # Errors
    ///
    /// Returns `None` if any plane is too small for `width` x `height`. A frame whose
    /// planes disagree with its geometry cannot be rendered correctly by anything, so it
    /// is refused at construction rather than misread later.
    #[must_use]
    #[allow(clippy::many_single_char_names)]
    pub fn from_planes(
        width: u32,
        height: u32,
        y: &[u8],
        u: &[u8],
        v: &[u8],
        timestamp_us: u64,
    ) -> Option<Self> {
        let w = usize::try_from(width).ok()?;
        let h = usize::try_from(height).ok()?;
        let (uv_w, uv_h) = (w.div_ceil(2), h.div_ceil(2));
        let luma = w.checked_mul(h)?;
        let chroma = uv_w.checked_mul(uv_h)?;
        if y.len() < luma || u.len() < chroma || v.len() < chroma {
            return None;
        }

        let mut data = Vec::with_capacity(luma.checked_add(chroma)?.checked_add(chroma)?);
        data.extend_from_slice(y.get(..luma)?);
        data.extend_from_slice(u.get(..chroma)?);
        data.extend_from_slice(v.get(..chroma)?);

        Some(Self {
            width,
            height,
            data,
            y_stride: w,
            uv_stride: uv_w,
            timestamp_us,
        })
    }

    /// Adopt a buffer that already holds the three planes, rows padded to their strides.
    ///
    /// This is the zero-copy path: `data` is taken as-is, so a JPEG decoder's output
    /// becomes a frame without any pixel being moved.
    ///
    /// # Errors
    ///
    /// Returns `None` if the strides are narrower than the geometry requires, or if the
    /// buffer is too small to hold all three planes at those strides.
    #[must_use]
    pub fn from_padded(
        width: u32,
        height: u32,
        data: Vec<u8>,
        y_stride: usize,
        uv_stride: usize,
        timestamp_us: u64,
    ) -> Option<Self> {
        let w = usize::try_from(width).ok()?;
        let h = usize::try_from(height).ok()?;
        let (uv_w, uv_h) = (w.div_ceil(2), h.div_ceil(2));
        if y_stride < w || uv_stride < uv_w {
            return None;
        }
        let needed = y_stride
            .checked_mul(h)?
            .checked_add(uv_stride.checked_mul(uv_h)?.checked_mul(2)?)?;
        if data.len() < needed {
            return None;
        }

        Some(Self {
            width,
            height,
            data,
            y_stride,
            uv_stride,
            timestamp_us,
        })
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    #[must_use]
    pub const fn timestamp_us(&self) -> u64 {
        self.timestamp_us
    }

    /// Bytes per row of the Y plane, which may exceed the width.
    #[must_use]
    pub const fn y_stride(&self) -> usize {
        self.y_stride
    }

    /// Bytes per row of each chroma plane, which may exceed half the width.
    #[must_use]
    pub const fn uv_stride(&self) -> usize {
        self.uv_stride
    }

    /// The Y plane, including any row padding. Read it `y_stride` bytes at a time.
    #[must_use]
    pub fn y(&self) -> &[u8] {
        let end = self.y_stride.saturating_mul(self.rows());
        self.data.get(..end).unwrap_or(&self.data)
    }

    /// The U plane, including any row padding. Read it `uv_stride` bytes at a time.
    #[must_use]
    pub fn u(&self) -> &[u8] {
        let start = self.y_stride.saturating_mul(self.rows());
        let end = start.saturating_add(self.chroma_bytes());
        self.data.get(start..end).unwrap_or_default()
    }

    /// The V plane, including any row padding. Read it `uv_stride` bytes at a time.
    #[must_use]
    pub fn v(&self) -> &[u8] {
        let start = self
            .y_stride
            .saturating_mul(self.rows())
            .saturating_add(self.chroma_bytes());
        let end = start.saturating_add(self.chroma_bytes());
        self.data.get(start..end).unwrap_or_default()
    }

    /// Rows in the Y plane.
    fn rows(&self) -> usize {
        usize::try_from(self.height).unwrap_or(0)
    }

    /// Rows in each chroma plane.
    fn chroma_rows(&self) -> usize {
        self.rows().div_ceil(2)
    }

    /// Bytes occupied by one chroma plane, padding included.
    fn chroma_bytes(&self) -> usize {
        self.uv_stride.saturating_mul(self.chroma_rows())
    }
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
        assert_eq!(
            PlaintextMedia::from_decrypted(raw.clone()).into_bytes(),
            raw
        );
        assert_eq!(WireMedia::from_network(raw.clone()).into_bytes(), raw);
        assert_eq!(
            PlaintextMedia::from_encoder(raw.clone()).as_bytes(),
            raw.as_slice()
        );
        assert_eq!(
            WireMedia::from_encrypted(raw.clone()).as_bytes(),
            raw.as_slice()
        );
    }

    /// Crossing the boundary in either direction is byte-preserving but requires naming
    /// the assertion being made, which is the entire point of the pair.
    #[test]
    fn explicit_boundary_crossings_preserve_bytes() {
        let raw = vec![9u8, 8, 7];
        let wire = WireMedia::from_network(raw.clone());
        assert_eq!(
            PlaintextMedia::assume_peer_sends_unencrypted(wire).as_bytes(),
            raw.as_slice()
        );

        let plain = PlaintextMedia::from_encoder(raw.clone());
        assert_eq!(
            WireMedia::deliberately_unencrypted(plain).as_bytes(),
            raw.as_slice()
        );
    }

    /// Length/emptiness accessors report the payload, not the wrapper.
    #[test]
    fn length_reflects_payload() {
        assert!(PlaintextMedia::from_encoder(Vec::new()).is_empty());
        assert_eq!(WireMedia::from_network(vec![0; 42]).len(), 42);
    }
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::expect_used,
    clippy::arithmetic_side_effects
)]
mod i420_frame_tests {
    use super::I420Frame;

    /// Padded rows must be readable at the frame's own stride, not at its width.
    ///
    /// A frame adopted from a decoder has padding; reading it at `width` shifts every row
    /// progressively and shears the picture into sliding bands. The accessors exist to
    /// make that unrepresentable, so this checks they return the padded planes intact.
    #[test]
    fn a_padded_frame_exposes_whole_padded_planes() {
        let (w, h) = (4_u32, 4_u32);
        let (y_stride, uv_stride) = (8_usize, 6_usize);
        let data = vec![0_u8; y_stride * 4 + uv_stride * 2 * 2];

        let frame =
            I420Frame::from_padded(w, h, data, y_stride, uv_stride, 0).expect("valid padded frame");

        assert_eq!(frame.y_stride(), y_stride);
        assert_eq!(frame.uv_stride(), uv_stride);
        assert_eq!(frame.y().len(), y_stride * 4, "whole padded luma plane");
        assert_eq!(frame.u().len(), uv_stride * 2);
        assert_eq!(frame.v().len(), uv_stride * 2);
    }

    /// The planes must not overlap: U starts where Y ends, V where U ends.
    #[test]
    fn the_planes_are_laid_out_consecutively() {
        let (y_stride, uv_stride) = (4_usize, 2_usize);
        let mut data = vec![0_u8; y_stride * 4 + uv_stride * 2 * 2];
        data[..y_stride * 4].fill(1);
        data[y_stride * 4..y_stride * 4 + uv_stride * 2].fill(2);
        data[y_stride * 4 + uv_stride * 2..].fill(3);

        let frame = I420Frame::from_padded(4, 4, data, y_stride, uv_stride, 0).expect("frame");

        assert!(frame.y().iter().all(|&s| s == 1), "Y plane");
        assert!(frame.u().iter().all(|&s| s == 2), "U plane");
        assert!(frame.v().iter().all(|&s| s == 3), "V plane");
    }

    /// A buffer too small for its claimed strides is refused, not read past its end.
    #[test]
    fn a_buffer_too_small_for_its_strides_is_refused() {
        assert!(I420Frame::from_padded(64, 64, vec![0; 100], 64, 32, 0).is_none());
    }

    /// A stride narrower than the geometry is a contradiction, not padding.
    #[test]
    fn a_stride_narrower_than_the_width_is_refused() {
        let data = vec![0_u8; 64 * 64 * 2];
        assert!(I420Frame::from_padded(64, 64, data.clone(), 32, 32, 0).is_none());
        assert!(I420Frame::from_padded(64, 64, data, 64, 8, 0).is_none());
    }

    /// Tightly-packed planes are the degenerate case of padded ones.
    #[test]
    fn planes_built_tightly_report_their_width_as_the_stride() {
        let frame = I420Frame::from_planes(8, 8, &[1; 64], &[2; 16], &[3; 16], 5).expect("frame");
        assert_eq!(frame.y_stride(), 8);
        assert_eq!(frame.uv_stride(), 4);
        assert_eq!(frame.y().len(), 64);
        assert_eq!(frame.timestamp_us(), 5);
    }
}
