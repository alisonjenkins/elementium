//! Run the capture-to-wire path in a tight loop, for a profiler to sample.
//!
//! Also reports the per-frame budget at several frame rates. A high target is a useful
//! magnifying glass: at 240fps the budget is 4.17ms for decode, downscale and encode
//! together, which is tight enough that anything wasteful shows up as a deficit rather
//! than as a number that looks fine in isolation. The rates a video call uses hide that --
//! at 30fps almost anything fits.
//!
//! `cargo bench` measures stages against each other; a profiler shows what is happening
//! *inside* the dominant one. The decode is roughly three quarters of the per-frame cost
//! and is a single call from our side, so which part of it dominates -- entropy decoding,
//! the inverse DCT, upsampling -- cannot be answered by timing our own code.
//!
//! ```bash
//! cargo build --release -p elementium-codec --example pipeline_profile
//! perf record -g --call-graph dwarf ./target/release/examples/pipeline_profile
//! perf report --stdio
//! ```

#![allow(
    clippy::print_stdout,
    clippy::expect_used,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::cast_lossless,
    clippy::cast_sign_loss,
    clippy::suboptimal_flops
)]

use elementium_codec::{Vp8Encoder, halve_i420, i420_to_rgba};
use elementium_types::I420Frame;

const W: u32 = 1280;
const H: u32 = 720;

/// A photograph-like image: mostly low-frequency, with mild noise. See the benchmark's
/// fixture note -- an incompressible image makes JPEG decoding Huffman-bound in a way no
/// camera frame is, and would send profiling to the wrong place.
fn sample_rgb(width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut out = vec![0_u8; w * h * 3];
    let mut seed = 0x9e37_79b9_u32;
    for y in 0..h {
        for x in 0..w {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = ((seed >> 26) as u8) as i16 - 32;
            let fx = x as f32 / w as f32;
            let fy = y as f32 / h as f32;
            let base = 120.0 + 90.0 * (1.0 - fy) * (0.35 + 0.65 * fx);
            let px = |v: f32| (v + f32::from(noise)).clamp(0.0, 255.0) as u8;
            let i = (y * w + x) * 3;
            out[i] = px(base * 1.05);
            out[i + 1] = px(base * 0.95);
            out[i + 2] = px(base * 0.88);
        }
    }
    out
}

fn main() {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(10);

    let rgb = sample_rgb(W, H);
    let mut jpeg = Vec::new();
    jpeg_encoder::Encoder::new(&mut jpeg, 85)
        .encode(&rgb, W as u16, H as u16, jpeg_encoder::ColorType::Rgb)
        .expect("fixture");
    println!("fixture: {}KB", jpeg.len() / 1024);

    let mut encoder = Vp8Encoder::new(W, H, 2764).expect("encoder");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let mut frames = 0_u64;
    // The app rate-limits the self-view independently of capture, because nothing consumes
    // it faster than the webview fetches it. Mirrored here or this measures work the
    // application does not do.
    let preview_gap = std::time::Duration::from_nanos(1_000_000_000 / 30);
    let mut last_preview = std::time::Instant::now()
        .checked_sub(preview_gap)
        .unwrap_or_else(std::time::Instant::now);

    while std::time::Instant::now() < deadline {
        let yuv = turbojpeg::decompress_to_yuv(&jpeg).expect("decode");
        let (w, h) = (yuv.width as u32, yuv.height as u32);
        let (y_stride, uv_stride) = (yuv.y_size().0, yuv.uv_size().0);
        let frame = I420Frame::from_padded(w, h, yuv.pixels, y_stride, uv_stride, 0)
            .expect("adopt decoder buffer");

        if last_preview.elapsed() >= preview_gap {
            last_preview = std::time::Instant::now();
            let _preview = halve_i420(&frame).as_ref().map(i420_to_rgba);
        }
        let _packets = encoder.encode(&frame).expect("encode");
        frames += 1;
    }

    let achieved = frames as f64 / seconds as f64;
    let per_frame_ms = (seconds as f64 * 1000.0) / frames as f64;

    println!("{frames} frames in {seconds}s ({achieved:.1} fps, {per_frame_ms:.2}ms per frame)");
    println!();
    println!("  target   budget    headroom");
    for target in [30.0_f64, 60.0, 120.0, 240.0] {
        let budget_ms = 1000.0 / target;
        let headroom = (budget_ms - per_frame_ms) / budget_ms * 100.0;
        let verdict = if headroom >= 0.0 { "ok" } else { "OVER" };
        println!("  {target:>5.0}fps {budget_ms:>6.2}ms  {headroom:>+6.1}%  {verdict}");
    }
}
