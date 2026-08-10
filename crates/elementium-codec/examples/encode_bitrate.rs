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

fn main() {
    for (label, width, height, target_kbps) in [
        ("720p at 1700 kbps (what livekit asks for)", 1280_u32, 720_u32, 1700_u32),
        ("720p at 2764 kbps (what bitrate_for picks)", 1280, 720, 2764),
        ("1080p at 4000 kbps", 1920, 1080, 4000),
    ] {
        let config = EncoderConfig {
            width,
            height,
            bitrate_kbps: target_kbps,
            max_framerate: FPS,
        };
        let Ok(mut encoder) = elementium_codec::NegotiatedEncoder::new(VideoCodec::Vp8, config)
        else {
            println!("{label}: encoder would not start");
            continue;
        };

        let mut bytes = 0_u64;
        let mut steady_bytes = 0_u64;
        let mut frames_out = 0_u32;
        let mut largest_keyframe = 0_u64;
        for i in 0..FRAMES {
            let frame = moving(width, height, i);
            match encoder.encode(&frame) {
                Ok(packets) => {
                    for packet in &packets {
                        let len = packet.data.as_bytes().len() as u64;
                        if packet.is_keyframe {
                            largest_keyframe = largest_keyframe.max(len);
                        }
                        bytes = bytes.saturating_add(len);
                        // After the first second, so the opening keyframe -- which is
                        // hundreds of kilobytes and unavoidable -- is not charged against a
                        // three-second average and read as rate control failing.
                        if i >= FPS {
                            steady_bytes = steady_bytes.saturating_add(len);
                        }
                        frames_out = frames_out.saturating_add(1);
                    }
                }
                Err(e) => {
                    println!("{label}: encode failed: {e}");
                    break;
                }
            }
        }

        let seconds = f64::from(FRAMES) / f64::from(FPS);
        let steady_seconds = f64::from(FRAMES.saturating_sub(FPS)) / f64::from(FPS);
        let kbps = (bytes as f64) * 8.0 / 1000.0 / seconds;
        let steady_kbps = (steady_bytes as f64) * 8.0 / 1000.0 / steady_seconds;
        let share = kbps / f64::from(target_kbps) * 100.0;
        let steady_share = steady_kbps / f64::from(target_kbps) * 100.0;
        // What one frame is worth at the target rate, so the keyframe can be stated in the
        // unit libvpx budgets in: a keyframe is always several frames' worth, and the
        // question is how many before the link has to absorb it.
        let frame_budget_bytes = f64::from(target_kbps) * 1000.0 / 8.0 / f64::from(FPS);
        let keyframe_budgets = largest_keyframe as f64 / frame_budget_bytes;
        println!(
            "{label}: asked {target_kbps} | whole run {kbps:.0} ({share:.0}%) | \
             after the keyframe {steady_kbps:.0} ({steady_share:.0}%) | {frames_out} packets | \
             largest keyframe {largest_keyframe} bytes ({keyframe_budgets:.0}x a frame's budget)"
        );
    }
}

/// A frame with content that changes every frame, so the encoder has real work to do.
///
/// A static picture compresses to almost nothing after its keyframe and would report a
/// flattering number that says nothing about a call.
#[allow(clippy::cast_possible_truncation, clippy::as_conversions, clippy::expect_used)]
fn moving(width: u32, height: u32, tick: u32) -> I420Frame {
    let w = width as usize;
    let h = height as usize;
    let shift = (tick as usize).wrapping_mul(7);
    let mut y = Vec::with_capacity(w.saturating_mul(h));
    for row in 0..h {
        for col in 0..w {
            // Diagonal bands that move, plus a coarse checker, which together defeat the
            // trivial temporal prediction a simple scroll would allow.
            let band = (row.wrapping_add(col).wrapping_add(shift) / 8) % 2;
            let checker = ((row / 32).wrapping_add(col / 32)) % 2;
            y.push(if band == checker { 40 } else { 215 });
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
