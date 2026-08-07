//! Measure the preview downscale, in whatever profile it is built with.
//!
//! Exists because this runs on the camera thread for every captured frame, and a cost that
//! is invisible in a release build can dominate a debug one -- which is what developers
//! and this project's `just dev` actually run.
//!
//! ```bash
//! cargo run -p elementium-codec --example halve_bench            # debug
//! cargo run --release -p elementium-codec --example halve_bench  # release
//! ```

#![allow(
    clippy::print_stdout,
    clippy::expect_used,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

use std::time::Instant;

fn main() {
    let (w, h) = (1280_u32, 720_u32);
    let src = vec![0x7f_u8; (w as usize) * (h as usize) * 4];
    let rounds = 60;

    let started = Instant::now();
    for _ in 0..rounds {
        let out = elementium_codec::halve_rgba(w, h, &src).expect("halve");
        std::hint::black_box(&out);
    }
    let per = started.elapsed() / rounds;

    println!(
        "halve_rgba {w}x{h}: {:.2}ms per frame ({:.0} fps ceiling){}",
        per.as_secs_f64() * 1000.0,
        1.0 / per.as_secs_f64(),
        if cfg!(debug_assertions) {
            "  [debug build]"
        } else {
            "  [release build]"
        },
    );
}
