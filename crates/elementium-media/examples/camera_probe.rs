//! Report what the camera stack can actually do on this machine.
//!
//! Camera failures here have been opaque: "Failed to open camera device" covers a missing
//! device, a device held by another process, a device that opens but has no usable format,
//! and a device that negotiates a format but never delivers a frame. Those need completely
//! different fixes, and the log line was the same for all of them.
//!
//! This walks every enumerated device and reports where each one stops, then captures a
//! short burst from the first that works and reports the real frame cadence and size.
//!
//! ```bash
//! cargo run -p elementium-media --example camera_probe
//! ```

use std::time::{Duration, Instant};

#[allow(
    clippy::print_stdout,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::arithmetic_side_effects
)]
fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    println!(
        "spa MediaSubtype: raw={} dsp={} mjpg={}",
        libspa::param::format::MediaSubtype::Raw.as_raw(),
        libspa::param::format::MediaSubtype::Dsp.as_raw(),
        libspa::param::format::MediaSubtype::Mjpg.as_raw()
    );
    println!("== PipeWire video sources ==");
    match elementium_media::pipewire_nodes::list_video_sources() {
        Ok(list) if list.is_empty() => println!("  none (is the pipewire daemon running?)"),
        Ok(list) => {
            for s in &list {
                println!(
                    "  node_id={} name={:?} desc={:?} path={:?}",
                    s.node_id, s.name, s.description, s.device_path
                );
            }
        }
        Err(e) => println!("  PipeWire unavailable: {e}"),
    }

    println!("\n== PipeWire capture ==");
    match elementium_media::pipewire_nodes::list_video_sources() {
        Ok(list) => {
            for s in &list {
                print!("  node {} ({}): ", s.node_id, s.description);
                match elementium_media::pipewire_capture::PipewireCapturer::start(s.node_id) {
                    Ok(cap) => {
                        let deadline = Instant::now() + Duration::from_secs(3);
                        let mut frames = 0_u32;
                        let mut started: Option<Instant> = None;
                        let mut latest: Option<Instant> = None;
                        let mut sample: Option<(u32, u32, usize)> = None;
                        while Instant::now() < deadline {
                            if let Some(f) = cap.try_recv() {
                                frames = frames.saturating_add(1);
                                let now = Instant::now();
                                started.get_or_insert(now);
                                latest = Some(now);
                                sample = Some((f.width(), f.height(), f.y().len()));
                                // Write one frame out so the pixels can be inspected
                                // directly. A capture that is geometrically or
                                // chromatically wrong looks identical to a healthy one in
                                // every scalar we log.
                                if frames == 30 && std::env::var_os("ELEMENTIUM_DUMP_FRAME").is_some() {
                                    let path = format!("/tmp/elementium_frame_{}.rgba", s.node_id);
                                    match std::fs::write(&path, elementium_codec::i420_to_rgba(&f).data) {
                                        Ok(()) => println!(
                                            "    wrote {path} ({}x{}, {} bytes)",
                                            f.width(), f.height(), f.y().len()
                                        ),
                                        Err(e) => println!("    frame dump failed: {e}"),
                                    }
                                }
                            } else {
                                std::thread::sleep(Duration::from_millis(2));
                            }
                        }
                        match (frames, sample, started, latest) {
                            (0, _, _, _) => println!("connected but delivered 0 frames in 3s"),
                            (count, Some((width, height, len)), Some(from), Some(to)) => {
                                let span = to.saturating_duration_since(from).as_secs_f64();
                                let fps = if span > 0.0 && count > 1 {
                                    f64::from(count.saturating_sub(1)) / span
                                } else {
                                    0.0
                                };
                                println!(
                                    "{count} frames in 3s ({fps:.1} fps), {width}x{height}, {len} bytes RGBA"
                                );
                            }
                            _ => println!("{frames} frames"),
                        }
                        cap.stop();
                    }
                    Err(e) => println!("FAILED: {e}"),
                }
            }
        }
        Err(e) => println!("  enumeration failed: {e}"),
    }

    println!("\n== V4L2 enumeration ==");
    match nokhwa::query(nokhwa::utils::ApiBackend::Auto) {
        Ok(list) if list.is_empty() => println!("  no cameras enumerated"),
        Ok(list) => {
            for info in &list {
                println!(
                    "  index={:?} name={:?} desc={:?}",
                    info.index(),
                    info.human_name(),
                    info.description()
                );
            }
        }
        Err(e) => println!("  enumeration failed: {e}"),
    }

    println!("\n== per-device open ==");
    let indices: Vec<u32> = nokhwa::query(nokhwa::utils::ApiBackend::Auto)
        .map(|l| l.iter().filter_map(|c| c.index().as_index().ok()).collect())
        .unwrap_or_default();

    for idx in &indices {
        print!("  /dev/video{idx}: ");
        match elementium_media::camera::CameraCapturer::start_with_index(*idx, None, None) {
            Ok(cap) => {
                println!("opened at {}x{}", cap.width(), cap.height());
                report_frames(&cap, *idx);
                cap.stop();
            }
            Err(e) => println!("FAILED: {e}"),
        }
    }

    if indices.is_empty() {
        println!("  (nothing to try)");
    }
}

/// Capture for a few seconds and report cadence, so a device that opens but never delivers
/// is distinguishable from one that works.
#[allow(clippy::print_stdout, clippy::arithmetic_side_effects)]
fn report_frames(cap: &elementium_media::camera::CameraCapturer, idx: u32) {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut frames = 0_u32;
    let mut bytes = 0_usize;
    let mut first_at: Option<Instant> = None;
    let mut last_at: Option<Instant> = None;

    while Instant::now() < deadline {
        if let Some(frame) = cap.try_recv() {
            frames = frames.saturating_add(1);
            bytes = bytes.saturating_add(frame.y().len());
            let now = Instant::now();
            first_at.get_or_insert(now);
            last_at = Some(now);
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    match (first_at, last_at) {
        (Some(first), Some(last)) if frames > 1 => {
            let span = last.saturating_duration_since(first).as_secs_f64();
            #[allow(clippy::cast_precision_loss)]
            let fps = if span > 0.0 { f64::from(frames.saturating_sub(1)) / span } else { 0.0 };
            println!(
                "    /dev/video{idx}: {frames} frames in 3s ({fps:.1} fps), {} bytes/frame avg",
                bytes.checked_div(usize::try_from(frames).unwrap_or(1)).unwrap_or(0)
            );
        }
        _ => println!("    /dev/video{idx}: opened but delivered {frames} frames in 3s"),
    }
}
