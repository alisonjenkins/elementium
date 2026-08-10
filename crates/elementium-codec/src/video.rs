//! The boundary between the media pipeline and whichever video codec is in use.
//!
//! The capture path — grab a frame, hand it to an encoder, put the result on the wire —
//! is the same whether the codec is VP8, VP9, AV1, H.264, or a hardware encoder reached
//! through VAAPI. Only the encoder differs, and which one is in use is decided at runtime
//! by SDP negotiation with the far end, not at compile time by us.
//!
//! So the pipeline talks to [`VideoEncoder`] and [`VideoDecoder`] rather than to VP8
//! directly. Adding a codec then means writing an implementation and listing it in
//! [`VideoCodec`]; it does not mean touching capture, the preview, the peer connection, or
//! anything else that handles frames.
//!
//! Dispatch is static throughout. Functions on the frame path take `impl VideoEncoder` or
//! a generic parameter, and the runtime choice of codec is carried by
//! [`NegotiatedEncoder`], an enum that implements the trait by matching. Nothing here
//! boxes, allocates per call, or goes through a vtable — this runs thirty times a second
//! for the length of every call, and the exhaustive enum additionally turns "a codec was
//! added but not handled" from a runtime surprise into a compile error.
//!
//! # What the traits deliberately do and do not assume
//!
//! **Frames are I420.** Every software video codec in use on the web takes planar YUV
//! 4:2:0, and it is what JPEG already stores, so it is the format that costs nothing to
//! arrive at. A hardware encoder wanting NV12 converts internally; when one exists and the
//! conversion shows up in a profile, [`VideoEncoder::preferred_input`] is where the
//! pipeline would learn to supply it directly.
//!
//! **Encoding produces zero or more packets.** Not one: a codec may buffer, emit several
//! partitions, or drop a frame entirely under its rate control, and a caller that assumes
//! one-in-one-out breaks on the first codec that does otherwise.
//!
//! **Keyframes are requested, not commanded.** A receiver asks for one over RTCP and the
//! encoder obliges on its next frame. Nothing here promises when.

use elementium_types::{I420Frame, PlaintextMedia};

use crate::codec_error::{Codec, CodecError, CodecErrorKind};

/// A video codec this application can negotiate.
///
/// The wire name is what appears in SDP, so it is part of the negotiation contract rather
/// than a display string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    /// Universally supported, and the only one with no hardware encoder anywhere.
    Vp8,
    /// What every GPU can encode, and what a hardware path realistically means.
    H264,
    /// Better compression than H.264 at the same quality; encoded in hardware by recent
    /// AMD, Intel and Apple parts, and decoded by fewer peers.
    Av1,
}

impl VideoCodec {
    /// Every codec this application knows about.
    ///
    /// An array rather than a runtime list so adding one is a compile error everywhere it
    /// has to be handled.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[Self::Vp8, Self::H264, Self::Av1]
    }

    /// The codecs there is a software encoder for.
    ///
    /// Distinct from [`VideoCodec::all`]: a codec can be negotiable and hardware-only, and
    /// offering one with no fallback would produce a call that dies the moment the GPU
    /// refuses.
    #[must_use]
    pub const fn software_supported() -> &'static [Self] {
        &[Self::Vp8]
    }

    /// Preference between codecs when hardware support does not decide it.
    ///
    /// Lower is preferred. VP8 ranks last despite being the safest choice: it is the
    /// fallback, and offering it first would have peers select it over codecs that cost
    /// this machine far less to encode.
    #[must_use]
    pub const fn negotiation_rank(self) -> u8 {
        match self {
            Self::Av1 => 0,
            Self::H264 => 1,
            Self::Vp8 => 2,
        }
    }

    /// The codec's name as it appears in SDP.
    #[must_use]
    pub const fn sdp_name(self) -> &'static str {
        match self {
            Self::Vp8 => "VP8",
            Self::H264 => "H264",
            Self::Av1 => "AV1",
        }
    }

    /// Match an RTP mime type, as the SFU reports its enabled codecs.
    ///
    /// The SFU sends `enabled_publish_codecs` as mime strings such as `video/H264`.
    /// Matching is case-insensitive because the case is not guaranteed and a mismatch
    /// would silently drop a codec the server does in fact allow.
    #[must_use]
    pub fn from_mime(mime: &str) -> Option<Self> {
        let name = mime.rsplit('/').next()?;
        Self::all()
            .iter()
            .copied()
            .find(|codec| codec.sdp_name().eq_ignore_ascii_case(name))
    }

    /// The RTP clock rate, in Hz.
    ///
    /// 90kHz for every video codec RTP carries; kept as a method rather than a constant
    /// because it belongs to the codec, and a future codec that differs should not require
    /// finding every place the number was assumed.
    #[must_use]
    pub const fn clock_rate(self) -> u32 {
        match self {
            Self::Vp8 | Self::H264 | Self::Av1 => 90_000,
        }
    }
}

/// The pixel layout an encoder wants its frames in.
///
/// Only I420 exists today. It is an enum rather than an implicit assumption so that a
/// hardware encoder wanting NV12 can say so, and the pipeline can convert once at the
/// source instead of the encoder converting on every frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelLayout {
    /// Planar YUV 4:2:0, three separate planes.
    I420,
    /// Semi-planar YUV 4:2:0: a luma plane and one interleaved chroma plane.
    ///
    /// What hardware encoders want, on every platform. Stating it here rather than having
    /// each backend convert internally means the conversion can happen once at the source
    /// -- or not at all, when the capture path can be asked to produce it directly.
    Nv12,
}

/// What an encoder is being asked to produce.
#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    /// Target bitrate in kbps. Adjustable later via [`VideoEncoder::set_bitrate`].
    pub bitrate_kbps: u32,
    /// The highest frame rate the caller will submit.
    ///
    /// Rate control needs this to size its budget per frame; it is not a promise to
    /// deliver that many.
    pub max_framerate: u32,
}

/// One encoded frame, ready to be encrypted and sent.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// Encoded bytes in the clear. See [`PlaintextMedia`] for why this is typed.
    pub data: PlaintextMedia,
    /// Whether this frame can be decoded without any earlier one.
    pub is_keyframe: bool,
    /// Presentation timestamp, in the codec's own units.
    pub pts: i64,
}

/// Anything that turns frames into encoded video.
///
/// `Send` because encoding happens on the capture thread, which is not the thread that
/// created the encoder.
pub trait VideoEncoder: Send {
    /// Which codec this is, for SDP and for the RTP payload type.
    fn codec(&self) -> VideoCodec;

    /// The geometry this encoder was configured for.
    ///
    /// Frames of any other size are rejected, so a caller whose source can renegotiate
    /// must check rather than assume.
    fn size(&self) -> (u32, u32);

    /// The layout frames must be supplied in.
    fn preferred_input(&self) -> PixelLayout {
        PixelLayout::I420
    }

    /// Encode one frame, returning whatever packets it produced.
    ///
    /// Zero packets is not an error: rate control may drop a frame, and a codec may buffer
    /// before emitting.
    ///
    /// # Errors
    ///
    /// Returns an error if the frame does not match [`VideoEncoder::size`], if its planes
    /// are too small for its geometry, or if the codec fails.
    fn encode(&mut self, frame: &I420Frame) -> Result<Vec<EncodedFrame>, CodecError>;

    /// Encode a JPEG directly, without decoding it first, if this encoder can.
    ///
    /// `None` means it cannot, and the caller must decode the frame and use
    /// [`VideoEncoder::encode`]. That is the honest shape: whether the compressed bytes can
    /// be used depends on the hardware — a GPU with a JPEG block and an encoder can take
    /// them, and everything else needs pixels — and a caller that assumed either way would
    /// be wrong on half the machines it runs on.
    ///
    /// Returning `Some(Err(..))` is different from returning `None`: it means the fast path
    /// exists and failed, which is worth reporting rather than silently retrying in
    /// software on every frame.
    ///
    /// # Errors
    ///
    /// Returns the encoder's error if the fast path exists and failed.
    fn encode_mjpeg(&mut self, _jpeg: &[u8]) -> Option<Result<Vec<EncodedFrame>, CodecError>> {
        None
    }

    /// Ask for the next frame to be a keyframe.
    ///
    /// Called when a receiver sends an RTCP PLI/FIR. A request, not a guarantee: the
    /// encoder decides when, and a caller that needs to know must look at
    /// [`EncodedFrame::is_keyframe`].
    fn request_keyframe(&mut self);

    /// Retarget the bitrate, in kbps.
    ///
    /// Calls are not fixed-bandwidth: congestion control lowers the target when the link
    /// degrades and raises it when it recovers, and a codec that cannot follow that wastes
    /// either quality or the user's connection.
    ///
    /// # Errors
    ///
    /// Returns an error if the codec rejects the new rate.
    fn set_bitrate(&mut self, kbps: u32) -> Result<(), CodecError>;
}

/// Anything that turns encoded video back into frames.
pub trait VideoDecoder: Send {
    /// Which codec this decodes.
    fn codec(&self) -> VideoCodec;

    /// Decode one packet, returning whatever frames it completed.
    ///
    /// Zero frames is normal: a frame may span several packets.
    ///
    /// # Errors
    ///
    /// Returns an error if the codec rejects the packet.
    fn decode(&mut self, data: &PlaintextMedia) -> Result<Vec<I420Frame>, CodecError>;
}

/// The encoder for whichever codec was negotiated.
///
/// Codec choice is a runtime decision — the far end says what it can decode — but that
/// does not require runtime dispatch. This wraps the concrete encoders in an enum that
/// implements [`VideoEncoder`] by matching, so every call is statically dispatched and
/// inlinable, and no allocation or vtable is involved on a path that runs thirty times a
/// second for the length of a call.
///
/// The enum also buys something `dyn` cannot: it is exhaustive. Adding a codec is a
/// compile error at every site that has to account for it, rather than something that
/// builds and then behaves oddly at runtime.
// One variant is a few hundred bytes larger than the other, and boxing to even them out
// would put a heap indirection on the encode path to save memory that is allocated once per
// call and freed when it ends. The whole reason this is an enum rather than a `dyn` is to
// keep that path free of pointer chasing.
#[allow(clippy::large_enum_variant)]
pub enum NegotiatedEncoder {
    Vp8(crate::vpx_codec::Vp8Encoder),
    /// H.264 on the GPU. Only where VAAPI is compiled in and a device offers it.
    #[cfg(all(target_os = "linux", feature = "vaapi"))]
    VaapiH264(crate::vaapi::H264Encoder),
}

impl NegotiatedEncoder {
    /// Build the encoder for `codec`.
    ///
    /// # Errors
    ///
    /// Returns an error if the encoder fails to initialise.
    pub fn new(codec: VideoCodec, config: EncoderConfig) -> Result<Self, CodecError> {
        match codec {
            VideoCodec::Vp8 => Ok(Self::Vp8(crate::vpx_codec::Vp8Encoder::new(
                config.width,
                config.height,
                config.bitrate_kbps,
                config.max_framerate,
            )?)),
            #[cfg(all(target_os = "linux", feature = "vaapi"))]
            VideoCodec::H264 => {
                crate::vaapi::H264Encoder::new(config.width, config.height, config.bitrate_kbps)
                    .map(Self::VaapiH264)
                    .map_err(|e| CodecError::new(Codec::H264Vaapi, CodecErrorKind::Vaapi(e)))
            }
            // Negotiable but not encodable here. Reported as an error rather than silently
            // substituting VP8: a caller that asked for H.264 has told the far end it will
            // send H.264, and sending something else produces a peer that receives
            // undecodable video with nothing to explain it.
            #[cfg(not(all(target_os = "linux", feature = "vaapi")))]
            VideoCodec::H264 => Err(CodecError::new(Codec::H264Vaapi, CodecErrorKind::Unsupported)),
            VideoCodec::Av1 => Err(CodecError::new(Codec::Av1, CodecErrorKind::Unsupported)),
        }
    }
}

impl VideoEncoder for NegotiatedEncoder {
    fn codec(&self) -> VideoCodec {
        match self {
            Self::Vp8(e) => e.codec(),
            #[cfg(all(target_os = "linux", feature = "vaapi"))]
            Self::VaapiH264(e) => e.codec(),
        }
    }

    fn size(&self) -> (u32, u32) {
        match self {
            Self::Vp8(e) => VideoEncoder::size(e),
            #[cfg(all(target_os = "linux", feature = "vaapi"))]
            Self::VaapiH264(e) => VideoEncoder::size(e),
        }
    }

    fn preferred_input(&self) -> PixelLayout {
        match self {
            Self::Vp8(e) => e.preferred_input(),
            #[cfg(all(target_os = "linux", feature = "vaapi"))]
            Self::VaapiH264(e) => e.preferred_input(),
        }
    }

    fn encode(&mut self, frame: &I420Frame) -> Result<Vec<EncodedFrame>, CodecError> {
        match self {
            Self::Vp8(e) => VideoEncoder::encode(e, frame),
            #[cfg(all(target_os = "linux", feature = "vaapi"))]
            Self::VaapiH264(e) => VideoEncoder::encode(e, frame),
        }
    }

    fn encode_mjpeg(&mut self, jpeg: &[u8]) -> Option<Result<Vec<EncodedFrame>, CodecError>> {
        match self {
            Self::Vp8(e) => e.encode_mjpeg(jpeg),
            #[cfg(all(target_os = "linux", feature = "vaapi"))]
            Self::VaapiH264(e) => e.encode_mjpeg(jpeg),
        }
    }

    fn request_keyframe(&mut self) {
        match self {
            Self::Vp8(e) => e.request_keyframe(),
            #[cfg(all(target_os = "linux", feature = "vaapi"))]
            Self::VaapiH264(e) => e.request_keyframe(),
        }
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<(), CodecError> {
        match self {
            Self::Vp8(e) => VideoEncoder::set_bitrate(e, kbps),
            #[cfg(all(target_os = "linux", feature = "vaapi"))]
            Self::VaapiH264(e) => VideoEncoder::set_bitrate(e, kbps),
        }
    }
}

/// The decoder for whichever codec was negotiated. See [`NegotiatedEncoder`].
pub enum NegotiatedDecoder {
    Vp8(crate::vpx_codec::Vp8Decoder),
    H264(crate::h264_codec::H264Decoder),
    #[cfg(all(target_os = "linux", feature = "vaapi"))]
    VaapiH264(crate::vaapi::h264_decode::H264Decoder),
}

impl NegotiatedDecoder {
    /// Build the decoder for `codec`.
    ///
    /// # Errors
    ///
    /// Returns an error if the decoder fails to initialise.
    pub fn new(codec: VideoCodec) -> Result<Self, CodecError> {
        match codec {
            VideoCodec::Vp8 => Ok(Self::Vp8(crate::vpx_codec::Vp8Decoder::new()?)),
            VideoCodec::H264 => {
                // Hardware first, software if it is not there. Decode is the side that
                // must never fail, so the fallback is not a fallback in name only: a
                // machine with no GPU, or a driver that encodes H.264 but will not decode
                // it, gets a working picture rather than a black tile.
                #[cfg(all(target_os = "linux", feature = "vaapi"))]
                if let Ok(hw) = crate::vaapi::h264_decode::H264Decoder::new() {
                    tracing::info!("H.264 will be decoded on the GPU");
                    return Ok(Self::VaapiH264(hw));
                }
                tracing::info!("H.264 will be decoded in software");
                Ok(Self::H264(crate::h264_codec::H264Decoder::new()?))
            }
            VideoCodec::Av1 => Err(CodecError::new(Codec::Av1, CodecErrorKind::Unsupported)),
        }
    }
}

impl VideoDecoder for NegotiatedDecoder {
    fn codec(&self) -> VideoCodec {
        match self {
            Self::Vp8(d) => d.codec(),
            Self::H264(d) => VideoDecoder::codec(d),
            #[cfg(all(target_os = "linux", feature = "vaapi"))]
            Self::VaapiH264(d) => VideoDecoder::codec(d),
        }
    }

    fn decode(&mut self, data: &PlaintextMedia) -> Result<Vec<I420Frame>, CodecError> {
        match self {
            Self::Vp8(d) => VideoDecoder::decode(d, data),
            Self::H264(d) => VideoDecoder::decode(d, data),
            #[cfg(all(target_os = "linux", feature = "vaapi"))]
            Self::VaapiH264(d) => VideoDecoder::decode(d, data),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::as_conversions,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]
mod tests {
    use super::{
        EncoderConfig, NegotiatedDecoder, NegotiatedEncoder, PixelLayout, VideoCodec, VideoDecoder,
        VideoEncoder,
    };
    use elementium_types::I420Frame;

    fn frame(width: u32, height: u32) -> I420Frame {
        let (w, h) = (width as usize, height as usize);
        let uv = (w / 2) * (h / 2);
        I420Frame::from_planes(
            width,
            height,
            &vec![128; w * h],
            &vec![128; uv],
            &vec![128; uv],
            0,
        )
        .expect("planes match the geometry")
    }

    fn config(width: u32, height: u32) -> EncoderConfig {
        EncoderConfig {
            width,
            height,
            bitrate_kbps: 500,
            max_framerate: 30,
        }
    }

    /// Written against the bound rather than a concrete type: this is how the pipeline
    /// uses an encoder, and compiling it proves the trait is usable generically. It is
    /// also what keeps the trait honest -- anything VP8-shaped that crept into the
    /// signature would fail to compile here first.
    fn drive<E: VideoEncoder>(encoder: &mut E, frame: &I420Frame) -> Vec<super::EncodedFrame> {
        encoder.encode(frame).expect("encode")
    }

    #[test]
    fn an_encoder_is_usable_through_its_trait_bound() {
        let mut encoder =
            NegotiatedEncoder::new(VideoCodec::Vp8, config(320, 240)).expect("encoder");

        assert_eq!(encoder.codec(), VideoCodec::Vp8);
        assert_eq!(VideoEncoder::size(&encoder), (320, 240));
        assert_eq!(encoder.preferred_input(), PixelLayout::I420);

        let packets = drive(&mut encoder, &frame(320, 240));
        assert!(
            packets.iter().any(|p| p.is_keyframe),
            "the first frame must be independently decodable"
        );

        VideoEncoder::set_bitrate(&mut encoder, 900).expect("retarget bitrate");
        encoder.request_keyframe();
        assert!(
            drive(&mut encoder, &frame(320, 240))
                .iter()
                .any(|p| p.is_keyframe),
            "a requested keyframe must arrive"
        );
    }

    #[test]
    fn a_decoder_decodes_what_the_encoder_produced() {
        let mut encoder =
            NegotiatedEncoder::new(VideoCodec::Vp8, config(320, 240)).expect("encoder");
        let mut decoder = NegotiatedDecoder::new(VideoCodec::Vp8).expect("decoder");
        assert_eq!(VideoDecoder::codec(&decoder), VideoCodec::Vp8);

        let packets = drive(&mut encoder, &frame(320, 240));
        let keyframe = packets
            .iter()
            .find(|p| p.is_keyframe)
            .expect("a keyframe to decode");
        let frames = VideoDecoder::decode(&mut decoder, &keyframe.data).expect("decode");

        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames.first().map(|f| (f.width(), f.height())),
            Some((320, 240))
        );
    }

    /// A frame of the wrong size must be refused, not encoded into a corrupt picture.
    #[test]
    fn a_frame_of_the_wrong_size_is_refused() {
        let mut encoder =
            NegotiatedEncoder::new(VideoCodec::Vp8, config(320, 240)).expect("encoder");
        assert!(VideoEncoder::encode(&mut encoder, &frame(640, 480)).is_err());
    }

    /// The SDP name is part of the negotiation contract, not a label.
    #[test]
    fn codecs_report_their_wire_identity() {
        assert_eq!(VideoCodec::Vp8.sdp_name(), "VP8");
        assert_eq!(VideoCodec::Vp8.clock_rate(), 90_000);
    }

    /// The SFU reports its enabled codecs as mime types, and the case is not guaranteed.
    /// A mismatch here silently drops a codec the server does allow.
    #[test]
    fn mime_types_map_back_to_codecs() {
        assert_eq!(VideoCodec::from_mime("video/VP8"), Some(VideoCodec::Vp8));
        assert_eq!(VideoCodec::from_mime("video/h264"), Some(VideoCodec::H264));
        assert_eq!(VideoCodec::from_mime("video/AV1"), Some(VideoCodec::Av1));
        assert_eq!(VideoCodec::from_mime("VP8"), Some(VideoCodec::Vp8));
        assert_eq!(VideoCodec::from_mime("audio/opus"), None);
        assert_eq!(VideoCodec::from_mime(""), None);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod decoder_selection_tests {
    use super::{NegotiatedDecoder, NegotiatedEncoder, VideoCodec, VideoDecoder};

    /// Whichever backend is chosen, it must decode the codec that was asked for.
    ///
    /// The selection is by availability, so this is one of the few places where the answer
    /// legitimately differs between machines -- which is exactly why the *contract* is
    /// asserted rather than the branch taken.
    #[test]
    fn the_chosen_h264_decoder_decodes_h264() {
        let decoder = NegotiatedDecoder::new(VideoCodec::H264).expect("some H.264 decoder");
        assert_eq!(VideoDecoder::codec(&decoder), VideoCodec::H264);
    }

    #[test]
    fn vp8_is_still_available() {
        let decoder = NegotiatedDecoder::new(VideoCodec::Vp8).expect("VP8 decoder");
        assert_eq!(VideoDecoder::codec(&decoder), VideoCodec::Vp8);
    }

    /// AV1 has no decoder on any path, and must say so rather than pick a wrong one -- with
    /// a specific `Unsupported` kind naming AV1, not a bare "it failed".
    #[test]
    fn av1_is_refused_rather_than_substituted() {
        // `expect_err` needs `NegotiatedDecoder: Debug`, which it deliberately is not (see
        // its doc comment on why it stays free of a vtable and any per-call overhead); a
        // `let else` reaches the error without that bound.
        let Err(err) = NegotiatedDecoder::new(VideoCodec::Av1) else {
            panic!("AV1 has no decoder");
        };
        assert_eq!(err.codec, super::Codec::Av1);
        assert!(matches!(err.kind, super::CodecErrorKind::Unsupported));
    }

    /// AV1 has no encoder either, and the same refusal shape applies on the encode side.
    #[test]
    fn av1_encoding_is_refused_rather_than_substituted() {
        let config = super::EncoderConfig {
            width: 320,
            height: 240,
            bitrate_kbps: 500,
            max_framerate: 30,
        };
        let Err(err) = NegotiatedEncoder::new(VideoCodec::Av1, config) else {
            panic!("AV1 has no encoder");
        };
        assert_eq!(err.codec, super::Codec::Av1);
        assert!(matches!(err.kind, super::CodecErrorKind::Unsupported));
    }
}
