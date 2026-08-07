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

/// A video codec this application can negotiate.
///
/// The wire name is what appears in SDP, so it is part of the negotiation contract rather
/// than a display string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    Vp8,
}

impl VideoCodec {
    /// The codec's name as it appears in SDP.
    #[must_use]
    pub const fn sdp_name(self) -> &'static str {
        match self {
            Self::Vp8 => "VP8",
        }
    }

    /// The RTP clock rate, in Hz.
    ///
    /// 90kHz for every video codec RTP carries; kept as a method rather than a constant
    /// because it belongs to the codec, and a future codec that differs should not require
    /// finding every place the number was assumed.
    #[must_use]
    pub const fn clock_rate(self) -> u32 {
        match self {
            Self::Vp8 => 90_000,
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
    fn encode(&mut self, frame: &I420Frame) -> Result<Vec<EncodedFrame>, String>;

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
    fn set_bitrate(&mut self, kbps: u32) -> Result<(), String>;
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
    fn decode(&mut self, data: &PlaintextMedia) -> Result<Vec<I420Frame>, String>;
}

/// Build an encoder for a negotiated codec.
///
/// The indirection is the point: the codec comes from SDP at runtime, so the choice cannot
/// be made where the pipeline is written.
///
/// # Errors
///
/// Returns an error if the codec is not supported or the encoder fails to initialise.
pub fn make_encoder(
    codec: VideoCodec,
    config: EncoderConfig,
) -> Result<Box<dyn VideoEncoder>, String> {
    match codec {
        VideoCodec::Vp8 => Ok(Box::new(crate::vpx_codec::Vp8Encoder::new(
            config.width,
            config.height,
            config.bitrate_kbps,
        )?)),
    }
}

/// Build a decoder for a negotiated codec.
///
/// # Errors
///
/// Returns an error if the codec is not supported or the decoder fails to initialise.
pub fn make_decoder(codec: VideoCodec) -> Result<Box<dyn VideoDecoder>, String> {
    match codec {
        VideoCodec::Vp8 => Ok(Box::new(crate::vpx_codec::Vp8Decoder::new()?)),
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
    use super::{EncoderConfig, PixelLayout, VideoCodec, make_decoder, make_encoder};
    use elementium_types::I420Frame;

    fn frame(width: u32, height: u32) -> I420Frame {
        let (w, h) = (width as usize, height as usize);
        let uv = (w / 2) * (h / 2);
        I420Frame {
            width,
            height,
            y: vec![128; w * h],
            u: vec![128; uv],
            v: vec![128; uv],
            timestamp_us: 0,
        }
    }

    /// The pipeline holds a `dyn VideoEncoder` because the codec is chosen by negotiation
    /// at runtime. A trait that is not object-safe cannot be used that way, and the failure
    /// would only appear when a second codec was added.
    #[test]
    fn an_encoder_can_be_held_and_driven_behind_a_trait_object() {
        let mut encoder = make_encoder(
            VideoCodec::Vp8,
            EncoderConfig {
                width: 320,
                height: 240,
                bitrate_kbps: 500,
                max_framerate: 30,
            },
        )
        .expect("encoder");

        assert_eq!(encoder.codec(), VideoCodec::Vp8);
        assert_eq!(encoder.size(), (320, 240));
        assert_eq!(encoder.preferred_input(), PixelLayout::I420);

        let packets = encoder.encode(&frame(320, 240)).expect("encode");
        assert!(
            packets.iter().any(|p| p.is_keyframe),
            "the first frame must be independently decodable"
        );

        encoder.set_bitrate(900).expect("retarget bitrate");
        encoder.request_keyframe();
        let after = encoder.encode(&frame(320, 240)).expect("encode");
        assert!(
            after.iter().any(|p| p.is_keyframe),
            "a requested keyframe must arrive"
        );
    }

    /// Decoders are chosen the same way and must be usable the same way.
    #[test]
    fn a_decoder_behind_a_trait_object_decodes_what_the_encoder_produced() {
        let mut encoder = make_encoder(
            VideoCodec::Vp8,
            EncoderConfig {
                width: 320,
                height: 240,
                bitrate_kbps: 500,
                max_framerate: 30,
            },
        )
        .expect("encoder");
        let mut decoder = make_decoder(VideoCodec::Vp8).expect("decoder");
        assert_eq!(decoder.codec(), VideoCodec::Vp8);

        let packets = encoder.encode(&frame(320, 240)).expect("encode");
        let keyframe = packets
            .iter()
            .find(|p| p.is_keyframe)
            .expect("a keyframe to decode");
        let frames = decoder.decode(&keyframe.data).expect("decode");

        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames.first().map(|f| (f.width, f.height)),
            Some((320, 240))
        );
    }

    /// A frame of the wrong size must be refused, not encoded into a corrupt picture.
    #[test]
    fn a_frame_of_the_wrong_size_is_refused() {
        let mut encoder = make_encoder(
            VideoCodec::Vp8,
            EncoderConfig {
                width: 320,
                height: 240,
                bitrate_kbps: 500,
                max_framerate: 30,
            },
        )
        .expect("encoder");
        assert!(encoder.encode(&frame(640, 480)).is_err());
    }

    /// The SDP name is part of the negotiation contract, not a label.
    #[test]
    fn codecs_report_their_wire_identity() {
        assert_eq!(VideoCodec::Vp8.sdp_name(), "VP8");
        assert_eq!(VideoCodec::Vp8.clock_rate(), 90_000);
    }
}
