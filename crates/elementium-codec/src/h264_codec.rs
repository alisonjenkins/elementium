//! Software H.264 decode, so a peer publishing H.264 is a picture rather than a black tile.
//!
//! The encoder side has had a VAAPI H.264 path for a while. The decoder side knew only VP8,
//! so `NegotiatedDecoder::new(VideoCodec::H264)` returned "no decoder available for H264 on
//! this build" and the track was received, decrypted, and thrown away.
//!
//! **Software, deliberately, even though the VAAPI plumbing already exists here.** Decode is
//! the side that must not fail: a machine with no GPU driver, or a driver that encodes H.264
//! but does not decode it, still has to show the picture. A decoder available only where the
//! encoder is would leave exactly the black tile this removes, on exactly the machines least
//! able to diagnose it.

use elementium_types::media::{I420Frame, PlaintextMedia};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use openh264::nal_units;

use crate::video::{VideoCodec, VideoDecoder};

/// An H.264 decoder.
pub struct H264Decoder {
    inner: Decoder,
    /// Set once the first frame has been produced, to log the geometry exactly once.
    reported: bool,
}

impl H264Decoder {
    /// Create a new H.264 decoder.
    ///
    /// # Errors
    ///
    /// Returns an error string if openh264 fails to initialise.
    pub fn new() -> Result<Self, String> {
        let inner = Decoder::new().map_err(|e| format!("H264 decoder init failed: {e}"))?;
        Ok(Self {
            inner,
            reported: false,
        })
    }

    /// Decode one packet, returning whatever frames it completed.
    ///
    /// # Errors
    ///
    /// Returns an error if the packet cannot be split into NAL units at all. A packet that
    /// merely fails to complete a picture is not an error -- see below.
    pub fn decode(&mut self, data: &PlaintextMedia) -> Result<Vec<I420Frame>, String> {
        let bytes = data.as_bytes();
        if bytes.is_empty() {
            return Ok(Vec::new());
        }

        let mut frames = Vec::new();
        // Fed one NAL unit at a time rather than as a whole access unit. openh264's
        // `decode` expects a single NAL and returns `Ok(None)` until it has enough to emit
        // a picture, which is also what makes parameter sets (SPS/PPS) arriving in their own
        // packets work without special-casing them.
        for nal in nal_units(bytes) {
            match self.inner.decode(nal) {
                Ok(Some(yuv)) => {
                    let (width, height) = yuv.dimensions();
                    let (pic_w, pic_h) = (
                        u32::try_from(width).map_err(|_| "H264 decode: implausible width")?,
                        u32::try_from(height).map_err(|_| "H264 decode: implausible height")?,
                    );
                    if !self.reported {
                        self.reported = true;
                        tracing::info!(width = pic_w, height = pic_h, "H264 decoder produced its first frame");
                    }
                    // Copied row by row, honouring openh264's strides.
                    //
                    // Its planes are padded: the stride is the allocation's width, which is
                    // wider than the picture. Handing those slices straight to
                    // `from_planes`, which packs rows at exactly `width`, skews every row
                    // after the first by the padding -- a picture that is the right size,
                    // the right colours and the wrong shape. Found by decoding the same
                    // stream in hardware and comparing, which is the only reason it was
                    // ever noticed: the frame looked entirely plausible on its own.
                    let (y_stride, u_stride, v_stride) = yuv.strides();
                    let packed = |plane: &[u8], stride: usize, width: usize, rows: usize| {
                        let mut out = Vec::with_capacity(width.saturating_mul(rows));
                        for row in 0..rows {
                            let start = row.saturating_mul(stride);
                            match plane.get(start..start.saturating_add(width)) {
                                Some(line) => out.extend_from_slice(line),
                                None => return None,
                            }
                        }
                        Some(out)
                    };
                    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
                    let (Some(luma), Some(chroma_b), Some(chroma_r)) = (
                        packed(yuv.y(), y_stride, width, height),
                        packed(yuv.u(), u_stride, cw, ch),
                        packed(yuv.v(), v_stride, cw, ch),
                    ) else {
                        return Err("H264 decode: a plane is smaller than its stride implies".to_owned());
                    };
                    let frame = I420Frame::from_planes(
                        pic_w,
                        pic_h,
                        &luma,
                        &chroma_b,
                        &chroma_r,
                        // Zero, as the VP8 decoder does. A decoded frame's presentation
                        // time comes from the RTP timestamp on the receive path, not from
                        // the codec, and inventing one here would put a second, disagreeing
                        // clock into the pipeline.
                        0,
                    )
                    .ok_or_else(|| {
                        format!("H264 decode: {pic_w}x{pic_h} planes too small for the stated geometry")
                    })?;
                    frames.push(frame);
                }
                // No picture yet. Normal: a frame spans several NAL units, and parameter
                // sets produce none at all. Treating this as an error would make every
                // healthy stream look broken.
                Ok(None) => {}
                Err(e) => {
                    // Logged and skipped rather than returned. A single corrupt NAL unit
                    // must not stop the stream: the next keyframe recovers it, and failing
                    // the whole packet would tear down a call over one lost packet.
                    tracing::debug!(
                        nal_len = nal.len(),
                        error = %e,
                        "H264 decode: skipping a NAL unit the decoder rejected"
                    );
                }
            }
        }
        Ok(frames)
    }
}

impl VideoDecoder for H264Decoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::H264
    }

    fn decode(&mut self, data: &PlaintextMedia) -> Result<Vec<I420Frame>, String> {
        Self::decode(self, data)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::H264Decoder;
    use crate::video::{VideoCodec, VideoDecoder};
    use elementium_types::media::PlaintextMedia;

    fn packet(bytes: &[u8]) -> PlaintextMedia {
        PlaintextMedia::from_decrypted(bytes.to_vec())
    }

    #[test]
    fn it_reports_the_codec_it_decodes() {
        let d = H264Decoder::new().expect("decoder");
        assert_eq!(VideoDecoder::codec(&d), VideoCodec::H264);
    }

    /// An empty packet is not an error and must not reach the decoder.
    #[test]
    fn an_empty_packet_produces_nothing() {
        let mut d = H264Decoder::new().expect("decoder");
        let out = d.decode(&packet(&[])).expect("empty packet is not an error");
        assert!(out.is_empty());
    }

    /// A packet that completes no picture is the normal case, not a failure.
    ///
    /// This is the distinction that matters most in this file: if "no frame yet" were an
    /// error, every healthy stream would report an error on most of its packets, and the
    /// real errors would be unreadable among them.
    #[test]
    fn a_packet_that_completes_no_picture_is_not_an_error() {
        let mut d = H264Decoder::new().expect("decoder");
        // A NAL unit with a valid start code whose payload completes nothing.
        let out = d
            .decode(&packet(&[0, 0, 0, 1, 0x67, 0x42, 0x00, 0x0a]))
            .expect("an incomplete picture is not an error");
        assert!(out.is_empty());
    }

    /// Garbage must be survivable. A corrupt NAL unit loses a frame, not the call.
    #[test]
    fn a_rejected_nal_unit_does_not_fail_the_packet() {
        let mut d = H264Decoder::new().expect("decoder");
        let out = d
            .decode(&packet(&[0, 0, 0, 1, 0xff, 0xff, 0xff, 0xff, 0xff]))
            .expect("a corrupt NAL unit must not fail the whole packet");
        assert!(out.is_empty());
    }
}

/// Round trip against a real encoder, which is the only test that proves a picture arrives.
///
/// Every other test in this file asserts a *negative* -- an empty packet, a NAL unit that
/// completes nothing, garbage. All of them would pass against a decoder that always returned
/// no frames, which is precisely the black tile this decoder exists to remove.
#[cfg(all(test, target_os = "linux", feature = "vaapi"))]
#[allow(clippy::expect_used, clippy::arithmetic_side_effects, clippy::as_conversions)]
mod roundtrip_tests {
    use super::H264Decoder;
    use crate::video::VideoEncoder;
    use elementium_types::media::I420Frame;

    const WIDTH: u32 = 320;
    const HEIGHT: u32 = 240;

    /// A frame with structure, not a flat colour: a flat frame survives almost any bug.
    fn test_frame() -> I420Frame {
        let (cols, rows) = (WIDTH as usize, HEIGHT as usize);
        let mut luma = vec![0u8; cols * rows];
        for (i, px) in luma.iter_mut().enumerate() {
            let (col, row) = (i % cols, i / cols);
            *px = if (col / 16 + row / 16) % 2 == 0 { 200 } else { 40 };
        }
        let chroma_b = vec![110u8; (cols / 2) * (rows / 2)];
        let chroma_r = vec![140u8; (cols / 2) * (rows / 2)];
        I420Frame::from_planes(WIDTH, HEIGHT, &luma, &chroma_b, &chroma_r, 0)
            .expect("fixture frame")
    }

    #[test]
    fn a_frame_from_the_hardware_encoder_decodes_back_to_a_picture() {
        let Ok(mut encoder) = crate::vaapi::H264Encoder::new(WIDTH, HEIGHT, 1_000) else {
            eprintln!("skipping: no VAAPI H.264 encoder on this machine");
            return;
        };
        let mut decoder = H264Decoder::new().expect("decoder");

        // Several frames: the first carries the parameter sets, and a decoder that only
        // works on frame one is a decoder that shows one picture and then freezes.
        let mut pictures = 0;
        for _ in 0..3 {
            let packets = VideoEncoder::encode(&mut encoder, &test_frame()).expect("encode");
            for packet in packets {
                let frames = decoder.decode(&packet.data).expect("decode");
                for frame in frames {
                    assert_eq!(frame.width(), WIDTH, "decoded width must match");
                    assert_eq!(frame.height(), HEIGHT, "decoded height must match");
                    pictures += 1;
                }
            }
        }
        assert!(
            pictures > 0,
            "the encoder produced packets but the decoder produced no picture"
        );
    }
}
