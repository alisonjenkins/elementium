//! Measure VP8 encode cost per frame at a realistic call resolution.
//!
//! Exists because "the encoder is slow" and "the machine is busy" are indistinguishable
//! from the app's logs, and the fix for one is not the fix for the other. Run it before
//! and after any change to the encoder's configuration.
//!
//! ```bash
//! cargo run --release -p elementium-codec --example encode_bench
//! ```

#![allow(
    clippy::print_stdout,
    clippy::expect_used,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

use std::time::{Duration, Instant};

use elementium_codec::Vp8Encoder;
use elementium_types::I420Frame;

fn main() {
    let (width, height) = (1280_u32, 720_u32);
    let frames = 120_u32;
    let mut encoder = Vp8Encoder::new(width, height, 2764, 30).expect("encoder");

    let w = width as usize;
    let h = height as usize;
    let uv = (w / 2) * (h / 2);

    // Moving content: a static scene encodes to almost nothing and would flatter the
    // encoder enormously, which is the opposite of what a benchmark is for.
    let mut convert_total = Duration::ZERO;
    let mut encode_total = Duration::ZERO;
    let mut worst = Duration::ZERO;
    let mut bytes = 0_usize;

    for i in 0..frames {
        let shift = (i % 255) as u8;
        // Camera frames arrive as RGBA, so the conversion is part of every frame's cost
        // and belongs in the measurement. Noise rather than a gradient: a smooth ramp
        // compresses to almost nothing and makes the encoder look far faster than it is
        // on real camera content.
        let mut rgba = vec![0_u8; w * h * 4];
        let mut seed = u32::from(shift).wrapping_add(1);
        for px in rgba.chunks_mut(4) {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            px[0] = (seed >> 16) as u8;
            px[1] = (seed >> 8) as u8;
            px[2] = seed as u8;
            px[3] = 255;
        }

        let started = Instant::now();
        let frame: I420Frame = elementium_codec::rgba_to_i420(width, height, &rgba);
        convert_total += started.elapsed();

        let started = Instant::now();
        let packets = encoder.encode(&frame).expect("encode");
        let elapsed = started.elapsed();
        encode_total += elapsed;
        worst = worst.max(elapsed);
        bytes += packets
            .iter()
            .map(|p| p.data.as_bytes().len())
            .sum::<usize>();
    }

    let convert = convert_total / frames;
    let encode = encode_total / frames;
    let per_frame = convert + encode;
    let _ = uv;
    println!("{width}x{height}, {frames} frames of noise");
    println!("  rgba->i420 {:>7.2}ms", convert.as_secs_f64() * 1000.0);
    println!(
        "  vp8 encode {:>7.2}ms (worst {:.2}ms)",
        encode.as_secs_f64() * 1000.0,
        worst.as_secs_f64() * 1000.0
    );
    println!(
        "  total      {:>7.2}ms per frame",
        per_frame.as_secs_f64() * 1000.0
    );
    println!("  {:.0} fps sustainable", 1.0 / per_frame.as_secs_f64());
    println!("  {} bytes per frame average", bytes / frames as usize);
}
