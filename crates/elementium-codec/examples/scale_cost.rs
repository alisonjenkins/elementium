//! What the per-frame downscale costs, at the sizes a call actually uses.
//!
//! `scale_i420` sits on the encode thread, once per frame, whenever the SFU asks for a
//! smaller picture than we capture. At 30fps a millisecond there is 3% of a core, and the
//! difference between an area average and a cheaper method is only worth arguing about
//! against a number.
//!
//! Reports milliseconds per frame for the ratios livekit asks for. Run it in release --
//! `just dev` builds unoptimised, and a debug figure would condemn the good implementation:
//!
//! ```text
//! nix develop -c cargo run --release -p elementium-codec --example scale_cost
//! ```

use std::time::Instant;

use elementium_types::I420Frame;

/// Enough frames that a single scheduling hiccup does not decide the answer.
const ROUNDS: u32 = 60;

fn main() {
    // A gradient rather than a flat field: a constant plane is the one input where a
    // sampling implementation and an averaging one cost the same, and where a compiler is
    // most able to help. This is closer to a picture.
    let cases = [
        ("1080p -> 720p  (scale 1.5)", 1920_u32, 1080_u32, 1280_u32, 720_u32),
        ("1080p -> 540p  (scale 2, exact half)", 1920, 1080, 960, 540),
        ("1080p -> 405p  (scale 2.667)", 1920, 1080, 720, 404),
        ("720p  -> 360p  (scale 2, exact half)", 1280, 720, 640, 360),
    ];

    for (label, width, height, out_width, out_height) in cases {
        let frame = gradient(width, height);
        // One outside the timer, so a cold allocator is not counted as scaling cost.
        let _warm = elementium_codec::scale_i420(&frame, out_width, out_height);

        let started = Instant::now();
        let mut produced = 0_u32;
        for _ in 0..ROUNDS {
            if let Some(scaled) = elementium_codec::scale_i420(&frame, out_width, out_height) {
                std::hint::black_box(&scaled);
                produced = produced.saturating_add(1);
            }
        }
        let per_frame = started.elapsed().as_secs_f64() * 1000.0 / f64::from(ROUNDS);
        let budget_pct = per_frame / (1000.0 / 30.0) * 100.0;
        println!(
            "{label}: {per_frame:.2}ms per frame ({budget_pct:.1}% of a 30fps frame budget), \
             {produced}/{ROUNDS} scaled"
        );
    }
}

/// A frame whose luma varies in both axes, so averaging has something to average.
#[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
fn gradient(width: u32, height: u32) -> I420Frame {
    let w = width as usize;
    let h = height as usize;
    let mut y = Vec::with_capacity(w.saturating_mul(h));
    for row in 0..h {
        for col in 0..w {
            y.push(((row.wrapping_mul(3)).wrapping_add(col) % 256) as u8);
        }
    }
    let uv_w = width.div_ceil(2) as usize;
    let uv_h = height.div_ceil(2) as usize;
    let chroma = vec![128_u8; uv_w.saturating_mul(uv_h)];
    #[allow(clippy::expect_used)]
    I420Frame::from_planes(width, height, &y, &chroma, &chroma, 0)
        .expect("a gradient frame of even geometry is valid")
}
