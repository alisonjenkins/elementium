//! Does the encoder produce the bitrate it was configured for?
//!
//! M2 in `specs/BACKLOG-2026-08-09-media.md` ends on exactly this question and calls it
//! untested: the encoder was created at 2764 kbps, `setParameters` asked for 1700, and the
//! measured rate on the wire was about 336. A picture starved that far at 720p is pixelated,
//! which is half of what the far end reported.
//!
//! This answers it without a call, a camera or a peer: encode a moving picture at a stated
//! cadence and add up the bytes libvpx hands back.
//!
//! Two pictures, because the answer depends on which. A camera-like picture is what a call
//! sends; the pathological one exists to show what happens when the encoder runs out of
//! quantizer and *cannot* obey its budget no matter what it is configured with. Reading only
//! the second one condemns settings that are working.
//!
//! ```text
//! nix develop -c cargo run --release -p elementium-codec --example encode_bitrate
//! ```

// A measurement harness: it counts bytes out of a real encoder and does arithmetic on them,
// which is what the casts and the short names below are. Scoped to this file rather than
// spread over the statements so the reason is stated once.
#![allow(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::many_single_char_names,
    clippy::arithmetic_side_effects
)]

use elementium_codec::video::{EncoderConfig, VideoCodec, VideoEncoder as _};
use elementium_types::I420Frame;

/// Three seconds at 30fps: long enough for rate control to settle past the first keyframe,
/// short enough to run in a moment.
const FRAMES: u32 = 90;
const FPS: u32 = 30;

/// When to ask for a keyframe, as a receiver that cannot decode would.
///
/// Two seconds in: past the opening keyframe and past the first second, so rate control has
/// settled and the keyframe is sized from the budget rather than from start-up state.
const KEYFRAME_REQUEST_FRAME: u32 = 60;

/// How hard the picture is to compress.
#[derive(Clone, Copy)]
enum Picture {
    /// Detail at the scale a camera produces it: gradients, gentle motion. The encoder has
    /// quantizer left to spend, so its rate control is the thing being measured.
    CameraLike,
    /// Fine detail everywhere, moving. The encoder saturates its quantizer and the frame is
    /// as small as VP8 can make it, which is still over budget -- a floor, not a failure.
    Pathological,
}

impl Picture {
    const fn label(self) -> &'static str {
        match self {
            Self::CameraLike => "camera-like",
            Self::Pathological => "pathological",
        }
    }
}

fn main() {
    for (label, width, height, target_kbps) in [
        ("720p at 1700 kbps (what livekit asks for)", 1280_u32, 720_u32, 1700_u32),
        ("720p at 2764 kbps (what bitrate_for picks)", 1280, 720, 2764),
        ("1080p at 4000 kbps", 1920, 1080, 4000),
    ] {
        for picture in [Picture::CameraLike, Picture::Pathological] {
            measure(label, width, height, target_kbps, picture);
        }
    }
}

/// Encode [`FRAMES`] frames of one picture at one target and report what came out.
fn measure(label: &str, width: u32, height: u32, target_kbps: u32, picture: Picture) {
    let config = EncoderConfig {
        width,
        height,
        bitrate_kbps: target_kbps,
        max_framerate: FPS,
    };
    let Ok(mut encoder) = elementium_codec::NegotiatedEncoder::new(VideoCodec::Vp8, config) else {
        println!("{label}: encoder would not start");
        return;
    };

    let mut bytes = 0_u64;
    let mut steady_bytes = 0_u64;
    let mut opening_keyframe = 0_u64;
    let mut requested_keyframe = 0_u64;
    for i in 0..FRAMES {
        // The keyframe M2 is about is not the opening one -- it is the one a receiver asks
        // for when it cannot decode, 192 times in the incident. Ask for one here so its size
        // is measured separately: libvpx sizes the first frame from the buffer it is given at
        // start-up, and a mid-stream keyframe from the budget.
        if i == KEYFRAME_REQUEST_FRAME {
            encoder.request_keyframe();
        }
        let packets = match encoder.encode(&moving(width, height, i, picture)) {
            Ok(packets) => packets,
            Err(e) => {
                println!("{label}: encode failed: {e}");
                return;
            }
        };
        for packet in &packets {
            let len = packet.data.as_bytes().len() as u64;
            if packet.is_keyframe {
                if i >= KEYFRAME_REQUEST_FRAME {
                    requested_keyframe = requested_keyframe.max(len);
                } else {
                    opening_keyframe = opening_keyframe.max(len);
                }
            }
            bytes = bytes.saturating_add(len);
            // After the first second, so the opening keyframe -- which is hundreds of
            // kilobytes and unavoidable -- is not charged against a three-second average and
            // read as rate control failing.
            if i >= FPS {
                steady_bytes = steady_bytes.saturating_add(len);
            }
        }
    }

    let seconds = f64::from(FRAMES) / f64::from(FPS);
    let steady_seconds = f64::from(FRAMES.saturating_sub(FPS)) / f64::from(FPS);
    let kbps = (bytes as f64) * 8.0 / 1000.0 / seconds;
    let steady_kbps = (steady_bytes as f64) * 8.0 / 1000.0 / steady_seconds;
    let share = kbps / f64::from(target_kbps) * 100.0;
    let steady_share = steady_kbps / f64::from(target_kbps) * 100.0;
    // What one frame is worth at the target rate, so the keyframe can be stated in the unit
    // libvpx budgets in: a keyframe is always several frames' worth, and the question is how
    // many before the link has to absorb it.
    let frame_budget_bytes = f64::from(target_kbps) * 1000.0 / 8.0 / f64::from(FPS);
    let opening_budgets = opening_keyframe as f64 / frame_budget_bytes;
    let requested_budgets = requested_keyframe as f64 / frame_budget_bytes;
    println!(
        "{label}, {picture}: asked {target_kbps} | whole run {kbps:.0} ({share:.0}%) | \
         after the keyframe {steady_kbps:.0} ({steady_share:.0}%) | \
         opening keyframe {opening_keyframe} bytes ({opening_budgets:.0}x a frame's budget) | \
         requested keyframe {requested_keyframe} bytes ({requested_budgets:.0}x)",
        picture = picture.label()
    );
}

/// A frame with content that changes every frame, so the encoder has real work to do.
///
/// A static picture compresses to almost nothing after its keyframe and would report a
/// flattering number that says nothing about a call.
#[allow(clippy::cast_possible_truncation, clippy::as_conversions, clippy::expect_used)]
fn moving(width: u32, height: u32, tick: u32, picture: Picture) -> I420Frame {
    let w = width as usize;
    let h = height as usize;
    let shift = (tick as usize).wrapping_mul(7);
    let mut y = Vec::with_capacity(w.saturating_mul(h));
    for row in 0..h {
        for col in 0..w {
            y.push(match picture {
                // A drifting gradient: detail the encoder can predict and quantize, which is
                // what a camera pointed at a person mostly is.
                Picture::CameraLike => {
                    ((row.wrapping_mul(3).wrapping_add(col).wrapping_add(shift)) / 4 % 256) as u8
                }
                // Diagonal bands that move, plus a coarse checker, which together defeat the
                // trivial temporal prediction a simple scroll would allow.
                Picture::Pathological => {
                    let band = (row.wrapping_add(col).wrapping_add(shift) / 8) % 2;
                    let checker = ((row / 32).wrapping_add(col / 32)) % 2;
                    if band == checker { 40 } else { 215 }
                }
            });
        }
    }
    let uv_w = width.div_ceil(2) as usize;
    let uv_h = height.div_ceil(2) as usize;
    let mut u = Vec::with_capacity(uv_w.saturating_mul(uv_h));
    for row in 0..uv_h {
        for col in 0..uv_w {
            u.push(((row.wrapping_add(col).wrapping_add(shift)) % 256) as u8);
        }
    }
    let v = vec![128_u8; uv_w.saturating_mul(uv_h)];
    I420Frame::from_planes(width, height, &y, &u, &v, u64::from(tick) * 33_333)
        .expect("a generated frame of even geometry is valid")
}
