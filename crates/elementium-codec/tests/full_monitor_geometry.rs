//! A whole monitor is not a webcam, and the encoder has to accept one.
//!
//! Screen sharing hands the encoder whatever geometry the compositor granted. On this
//! machine that is a 5120x1440 monitor -- seven and a third megapixels, ten times a 720p
//! webcam frame and wider than any dimension the camera path had ever been given. VP8 has
//! its own limits on frame size, and an encoder that refuses the geometry fails at the
//! first frame of a share, long after the picker, the portal and the publish have all
//! reported success. That failure reads as a transport fault at the far end.
//!
//! Asserted here rather than through the capture path because reaching it that way needs a
//! person to pick a monitor in a portal dialog, which no test can do.

// A failed setup step in a test should stop that test loudly and immediately; the
// workspace's `expect_used` ban is aimed at the shipping paths, not at assertions.
#![allow(clippy::expect_used)]

use elementium_codec::{EncoderConfig, NegotiatedEncoder, VideoCodec, VideoEncoder};
use elementium_types::I420Frame;

/// The widest single display in use here, and the case that motivated this file.
const ULTRAWIDE: (u32, u32) = (5120, 1440);

/// A frame of flat grey, which is all this needs: the assertion is about geometry.
fn grey_frame(width: u32, height: u32) -> I420Frame {
    let w = usize::try_from(width).expect("width fits");
    let h = usize::try_from(height).expect("height fits");
    let luma = vec![128_u8; w.saturating_mul(h)];
    let chroma = vec![128_u8; w.div_ceil(2).saturating_mul(h.div_ceil(2))];
    I420Frame::from_planes(width, height, &luma, &chroma, &chroma, 0).expect("frame builds")
}

/// The encoder must accept a full ultrawide monitor and produce a decodable keyframe.
#[test]
fn vp8_encodes_a_full_ultrawide_monitor() {
    let (width, height) = ULTRAWIDE;
    let mut encoder = NegotiatedEncoder::new(
        VideoCodec::Vp8,
        EncoderConfig { width, height, bitrate_kbps: 4000, max_framerate: 30 },
    )
    .expect("VP8 must initialise at full monitor geometry");

    assert_eq!(encoder.size(), (width, height));

    let packets = encoder
        .encode(&grey_frame(width, height))
        .expect("a full-monitor frame must encode");

    // The first frame is always a keyframe, and a share whose first frame is not one shows
    // nothing until the next -- which, on a damage-driven screencast of a static window,
    // may be a long time away.
    assert!(!packets.is_empty(), "the first frame must produce output");
    assert!(
        packets.iter().any(|p| p.is_keyframe),
        "the first encoded frame must be a keyframe"
    );
}

/// A frame whose geometry does not match the encoder's must be refused, not reinterpreted.
///
/// A screencast can renegotiate mid-share -- a window resizes, a monitor mode changes --
/// and an encoder that accepted the new size against its old configuration would read the
/// planes at the wrong stride and emit a sheared picture at a perfectly healthy frame rate.
#[test]
fn a_frame_that_does_not_match_the_negotiated_size_is_refused() {
    let (width, height) = ULTRAWIDE;
    let mut encoder = NegotiatedEncoder::new(
        VideoCodec::Vp8,
        EncoderConfig { width, height, bitrate_kbps: 4000, max_framerate: 30 },
    )
    .expect("VP8 must initialise at full monitor geometry");

    let resized = grey_frame(1880, 1446);
    assert!(
        encoder.encode(&resized).is_err(),
        "a frame of different geometry must be refused rather than misread"
    );
}
