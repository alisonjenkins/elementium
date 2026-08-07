//! Reproduce the app's local-preview pipeline and dump what it produces.
//!
//! The camera probe shows raw capture is clean and the app's preview is torn. The steps
//! between them are: the same `VideoSource`, a clone into the shared frame buffer, and the
//! halving applied before IPC. This runs exactly those, so a dump from here either
//! reproduces the corruption -- putting it in Rust -- or does not, putting it in the
//! webview.
//!
//! ```bash
//! cargo run --release -p elementium-media --example preview_probe
//! ```
//!
//! Writes `/tmp/elementium_preview_probe_<n>_<W>x<H>.rgba`.

#![allow(
    clippy::print_stdout,
    clippy::expect_used,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::or_fun_call
)]

use std::time::{Duration, Instant};

use elementium_codec::Vp8Encoder;
use elementium_media::video_source::VideoSource;

fn main() {
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();

    let source = VideoSource::start(Some(1280), Some(720)).expect("camera");
    println!("backend: {}", source.backend());

    // The app VP8-encodes every frame as well as previewing it, which the first version of
    // this probe did not -- so it drained the capture far faster than the app does and
    // came back clean while the app was producing banded frames. Matching the app's real
    // per-frame cost is the whole point of a reproduction.
    let mut encoder: Option<Vp8Encoder> = None;

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut frames = 0_u32;
    let mut dumped = 0_u32;

    while Instant::now() < deadline && dumped < 3 {
        let Some(frame) = source.try_recv() else {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        };
        frames += 1;

        // Exactly what the app does before the frame crosses into the webview: halve in
        // I420, then convert the smaller frame to RGBA.
        let preview = elementium_codec::halve_i420(&frame)
            .as_ref()
            .map_or_else(
                || elementium_codec::i420_to_rgba(&frame),
                elementium_codec::i420_to_rgba,
            );
        let (rgba, width, height) = (preview.data, preview.width, preview.height);

        let expected = (width as usize) * (height as usize) * 4;
        if rgba.len() != expected {
            println!(
                "MISMATCH at frame {frames}: {width}x{height} claims {expected} bytes, buffer has {}",
                rgba.len()
            );
        }

        if encoder
            .as_ref()
            .is_none_or(|e| e.size() != (frame.width(), frame.height()))
        {
            encoder = Vp8Encoder::new(frame.width(), frame.height(), 2764).ok();
        }
        if let Some(enc) = encoder.as_mut() {
            // No conversion: capture already produces the encoder's input format.
            let _ = enc.encode(&frame);
        }

        if frames.is_multiple_of(60) {
            let path =
                format!("/tmp/elementium_preview_probe_{frames}_{width}x{height}.rgba");
            std::fs::write(&path, &rgba).expect("write dump");
            println!("wrote {path} ({} bytes, source {}x{})", rgba.len(), frame.width(), frame.height());
            dumped += 1;
        }
    }

    source.stop();
    println!("{frames} frames seen, {dumped} dumped");
}
