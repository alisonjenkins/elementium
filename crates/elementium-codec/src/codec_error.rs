//! The shared error surface for [`crate::video::VideoEncoder`] and
//! [`crate::video::VideoDecoder`].
//!
//! One type serves both traits, per the constitution's Principle I: every caller on the
//! frame path is generic over the trait and handles every failure identically -- log it,
//! drop the frame, keep going -- so an associated type would only force callers to be
//! generic themselves or to box for no gain, and a type per implementation would leak the
//! codec choice back into every call site that is written to not care which codec it holds.
//!
//! What a shared type must not lose is *which codec* failed: VP8 and VAAPI H.264 fail for
//! entirely different reasons, on entirely different paths, and a dropped-frame log that
//! cannot tell them apart is a step backwards from the strings this replaces. So [`Codec`]
//! is carried as its own field alongside [`CodecErrorKind`] -- the same shape
//! [`crate::vaapi::status::Status`] already uses for "which libva call, and why", generalised
//! here to "which codec, and why".

use std::fmt;

use thiserror::Error;

#[cfg(all(target_os = "linux", feature = "vaapi"))]
use crate::vaapi::Status;

/// Which codec implementation produced a [`CodecError`].
///
/// Distinct from [`crate::video::VideoCodec`]: H.264 has two implementations in this crate
/// (software, via openh264, and hardware, via VAAPI), and they fail in unrelated ways. A log
/// line that only said "H264 failed" would still leave a reader guessing which one refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Vp8,
    /// The openh264-backed decoder. There is no software H.264 *encoder* in this crate.
    H264Software,
    /// The VAAPI-backed encoder and decoder.
    H264Vaapi,
    /// Negotiable, but nothing here implements it either way.
    Av1,
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Vp8 => "VP8",
            Self::H264Software => "H.264 (software)",
            Self::H264Vaapi => "H.264 (VAAPI)",
            Self::Av1 => "AV1",
        })
    }
}

/// A newtype over libvpx's raw status code.
///
/// `vpx_codec_err_t` is a bindgen-generated type from another crate, and `std::error::Error`
/// is a trait from `std` -- the orphan rule forbids implementing one for the other directly,
/// which is the whole reason this exists. `{:?}` is what every call site already logged
/// (`vpx_error_code = ?ret`), so `Display` reuses it rather than inventing a second string
/// for the same code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VpxStatus(pub vpx_sys::vpx_codec_err_t);

impl fmt::Display for VpxStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl std::error::Error for VpxStatus {}

/// Every distinct way an encode or a decode can fail, across every codec.
///
/// Several of these used to be folded into one `format!` per function -- a size mismatch,
/// short planes, a null `vpx_img_wrap`, and a genuine libvpx failure were all just "VP8
/// encode: {ret:?}" or worse. Splitting them here is what makes each one specific rather
/// than an admission that the failure was never thought through (Principle I).
#[derive(Error, Debug)]
pub enum CodecErrorKind {
    /// The codec was negotiated but nothing in this build implements it: AV1 always, or
    /// H.264 on a build without the `vaapi` feature.
    #[error("no implementation is compiled in for this codec on this build")]
    Unsupported,

    /// libvpx requires even dimensions for 4:2:0 chroma.
    #[error("requires even dimensions, got {width}x{height}")]
    OddDimensions { width: u32, height: u32 },

    /// `vpx_codec_enc_config_default` refused to produce a default configuration.
    #[error("could not get a default encoder configuration: {0}")]
    DefaultConfig(#[source] VpxStatus),

    /// `vpx_codec_enc_init_ver` refused to start the encoder.
    #[error("encoder initialisation failed: {0}")]
    EncoderInit(#[source] VpxStatus),

    /// `vpx_codec_dec_init_ver` refused to start the decoder.
    #[error("decoder initialisation failed: {0}")]
    DecoderInit(#[source] VpxStatus),

    /// `vpx_codec_enc_config_set` refused a retargeted bitrate.
    #[error("could not retarget bitrate to {kbps}kbps: {source}")]
    SetBitrate { kbps: u32, source: VpxStatus },

    /// A frame's geometry does not match what the encoder was configured for.
    #[error("frame size mismatch: encoder is {}x{}, frame is {}x{}", expected.0, expected.1, actual.0, actual.1)]
    SizeMismatch {
        expected: (u32, u32),
        actual: (u32, u32),
    },

    /// A frame's planes are shorter than its own stated geometry requires.
    #[error("I420 planes too small for {width}x{height}")]
    PlanesTooSmall { width: u32, height: u32 },

    /// `vpx_img_wrap` returned null.
    #[error("could not wrap the I420 frame for libvpx")]
    ImageWrapFailed,

    /// `vpx_codec_encode` refused the frame.
    #[error("encode failed: {0}")]
    Encode(#[source] VpxStatus),

    /// `vpx_codec_decode` refused the packet.
    #[error("decode failed: {0}")]
    Decode(#[source] VpxStatus),

    /// A packet longer than libvpx's `u32` length field.
    #[error("packet of {len} bytes exceeds the maximum libvpx will take")]
    PacketTooLarge { len: usize },

    /// libvpx handed back a decoded image whose own structure this crate could not read --
    /// a missing stride, a missing plane, or geometry that will not fit `usize`. Every
    /// branch here is defensive: the driver is not documented to do any of this, and none
    /// has been reproduced. Grouped under one variant with the specific field named, the
    /// same shape [`Status`] uses for "which libva call", because a dozen one-field variants
    /// for equally untestable driver misbehaviour would not be more informative than this.
    #[error("libvpx returned a decoded image with unreadable {field}")]
    MalformedVpxImage { field: &'static str },

    /// A `u32` width or height from the decoder did not fit where it was needed.
    #[error("implausible {field}: {source}")]
    ImplausibleGeometry {
        field: &'static str,
        #[source]
        source: std::num::TryFromIntError,
    },

    /// openh264 could not decode a NAL unit's picture: a plane the decoder returned is
    /// smaller than the stride it also reported for the same plane.
    #[error("a decoded plane is smaller than its own reported stride")]
    PlaneStrideMismatch,

    /// The picture openh264 produced does not fit the geometry it also reported.
    #[error("{width}x{height} planes too small for the stated geometry")]
    GeometryMismatch { width: u32, height: u32 },

    /// openh264 failed outright -- decoder construction, or a NAL unit it rejected.
    #[error("openh264 error: {0}")]
    Openh264(#[source] openh264::Error),

    /// Any failure on the VAAPI path: driver calls, and the small number of things this
    /// crate checks itself before making one. See [`Status`] for how those two are told
    /// apart without losing either one's cause.
    #[cfg(all(target_os = "linux", feature = "vaapi"))]
    #[error("{0}")]
    Vaapi(#[source] Status),
}

/// A failure on the video encode/decode hot path: which codec, and why.
///
/// The trait boundary both [`crate::video::VideoEncoder`] and [`crate::video::VideoDecoder`]
/// share, so every caller on the frame path -- which handles every failure identically, by
/// logging it and dropping the frame -- has exactly one error type to match regardless of
/// which codec produced it.
#[derive(Error, Debug)]
#[error("{codec} {kind}")]
pub struct CodecError {
    pub codec: Codec,
    #[source]
    pub kind: CodecErrorKind,
}

impl CodecError {
    /// Build a [`CodecError`] from its two parts.
    ///
    /// A plain constructor rather than a public struct literal so call sites read
    /// `CodecError::new(Codec::Vp8, CodecErrorKind::Encode(status))` -- codec and kind
    /// together, never one without the other.
    #[must_use]
    pub const fn new(codec: Codec, kind: CodecErrorKind) -> Self {
        Self { codec, kind }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{Codec, CodecError, CodecErrorKind, VpxStatus};

    /// The codec must be legible in the rendered message: that is the entire reason this
    /// type carries it as a field rather than losing it the way a bare `String` did.
    #[test]
    fn the_message_names_the_codec() {
        let err = CodecError::new(
            Codec::H264Vaapi,
            CodecErrorKind::SizeMismatch {
                expected: (640, 480),
                actual: (320, 240),
            },
        );
        let rendered = err.to_string();
        assert!(rendered.contains("H.264 (VAAPI)"), "got {rendered}");
        assert!(rendered.contains("640x480"), "got {rendered}");
    }

    /// The real cause must still be reachable through the error chain, not just folded into
    /// the message -- a caller that wants to match on it, not just log it, needs `source()`.
    #[test]
    fn a_wrapped_vpx_status_is_reachable_as_the_source() {
        use std::error::Error as _;

        let err = CodecError::new(
            Codec::Vp8,
            CodecErrorKind::Encode(VpxStatus(vpx_sys::vpx_codec_err_t::VPX_CODEC_ERROR)),
        );
        assert!(err.source().is_some(), "the VpxStatus must be reachable");
    }
}
