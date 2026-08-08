//! Where do the missing frames go when a camera runs at 30fps?
//!
//! "Capture is running below the requested rate" has three quite different causes: the
//! camera delivering fewer buffers than asked for, us dropping them to hold a rate, or the
//! consumer being too slow to take them. From the far end they look identical, and from a
//! frame counter they look identical too.
//!
//! The capture path already counts each separately. This runs long enough for those
//! counters to report -- they reset every 300 frames, which is ten seconds at 30fps -- and
//! then attributes the shortfall rather than leaving it as a feeling that the picture is
//! choppy.
//!
//! ```bash
//! cargo run -p elementium-media --example capture_attribution
//! ```
//!
//! **This opens the camera.** The light comes on for the duration of the run.

use std::time::{Duration, Instant};

#[allow(clippy::arithmetic_side_effects)]
/// The first enumerated source that actually delivers a frame.
///
/// Not simply the first source: this machine enumerates a virtual V4L2 node ahead of the
/// real camera which negotiates a format and then delivers nothing, and measuring that one
/// looks exactly like a camera producing no frames. Starting is not delivering -- the
/// failure is reported asynchronously, a moment after `start_at` returns -- so waiting for
/// a frame is the only test that tells them apart.
fn first_delivering_source(
    sources: &[elementium_media::pipewire_nodes::PipewireVideoSource],
    target: elementium_codec::EncodeTarget,
) -> Option<elementium_media::pipewire_capture::PipewireCapturer> {
    for source in sources {
        println!("trying node {} ({})", source.node_id, source.description);
        match elementium_media::pipewire_capture::PipewireCapturer::start_at(
            source.node_id,
            TARGET_FPS,
            target,
        ) {
            Ok(capturer) => {
                let wait = Instant::now() + Duration::from_secs(2);
                let mut first = None;
                while Instant::now() < wait && first.is_none() {
                    first = capturer.try_recv();
                    if first.is_none() {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
                if first.is_some() {
                    println!("  delivering; measuring this one");
                    return Some(capturer);
                }
                println!("  started but delivered nothing in 2s; trying the next");
            }
            Err(e) => println!("  could not start: {e}"),
        }
    }
    None
}

#[allow(clippy::cast_precision_loss, clippy::as_conversions)]
/// CPU time this process has used, user plus system, in seconds.
///
/// Read from `/proc/self/stat` because the interesting number is what the whole capture
/// costs the machine, not what one timed step costs. `mean_ms` in the capture log measures
/// the decode call alone; a path that moves work to the GPU could still spend it elsewhere,
/// and only total CPU would show that.
fn cpu_seconds() -> f64 {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return 0.0;
    };
    // Fields after the executable name, which may itself contain spaces inside parentheses.
    let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
        return 0.0;
    };
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // utime and stime are fields 14 and 15 of the full line; the first two are consumed by
    // the pid and the name, so they sit at 11 and 12 here.
    let ticks = |i: usize| fields.get(i).and_then(|f| f.parse::<u64>().ok()).unwrap_or(0);
    let clock_ticks = 100.0;
    ticks(11).saturating_add(ticks(12)) as f64 / clock_ticks
}

const TARGET_FPS: u32 = 30;
const SECONDS: u64 = 15;

#[allow(
    clippy::print_stdout,
    clippy::expect_used,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::as_conversions
)]
fn main() {
    // Which capture target to measure. The two differ in what the CPU does per frame: the
    // software path decodes MJPEG on the CPU and converts to I420, the hardware path hands
    // the JPEG to the GPU and asks for NV12 back. The claim this checks is that the second
    // costs materially less CPU per frame -- which is the whole reason for the hardware
    // path, and which nothing had measured.
    let hardware = std::env::args().any(|a| a == "--hardware");
    let target = if hardware {
        elementium_codec::EncodeTarget::negotiated(elementium_codec::VideoCodec::H264, 1280, 720)
    } else {
        elementium_codec::EncodeTarget::software()
    };

    // `info`, because the attribution this exists to read is logged at info by the capture
    // path itself -- the example's own numbers are only the consumer's half of it.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let sources = match elementium_media::pipewire_nodes::list_video_sources() {
        Ok(list) if !list.is_empty() => list,
        Ok(_) => {
            println!("no video sources; nothing to measure");
            return;
        }
        Err(e) => {
            println!("could not enumerate video sources: {e}");
            return;
        }
    };
    let Some(capturer) = first_delivering_source(&sources, target) else {
        println!("no source delivered a frame; nothing to measure");
        return;
    };
    let cpu_before = cpu_seconds();
    let deadline = Instant::now() + Duration::from_secs(SECONDS);
    let mut received = 0_u64;
    let mut first_at: Option<Instant> = None;
    let mut last_at: Option<Instant> = None;
    while Instant::now() < deadline {
        if let Some(_frame) = capturer.try_recv() {
            received += 1;
            let now = Instant::now();
            first_at.get_or_insert(now);
            last_at = Some(now);
        } else {
            // Short enough not to be the reason a frame is late, long enough not to spin.
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    let span = match (first_at, last_at) {
        (Some(first), Some(last)) => last.saturating_duration_since(first).as_secs_f64(),
        _ => 0.0,
    };
    let delivered_fps = if span > 0.0 {
        (received.saturating_sub(1)) as f64 / span
    } else {
        0.0
    };
    let expected = f64::from(TARGET_FPS) * span;

    let cpu_used = cpu_seconds() - cpu_before;
    println!();
    println!("  frames the consumer received: {received}");
    println!("  process CPU during capture: {cpu_used:.2}s");
    if received > 0 {
        let per_frame_ms = cpu_used * 1000.0 / received as f64;
        println!("  process CPU per frame: {per_frame_ms:.2}ms");
    }
    println!("  over: {span:.1}s");
    println!("  delivered rate: {delivered_fps:.1}fps against {TARGET_FPS} requested");
    println!("  expected at the requested rate: {expected:.0}");
    println!();
    println!("The `capture decode cost` lines above carry the attribution:");
    println!("  offered      -- buffers the camera actually handed us");
    println!("  rate_limited -- dropped by us to hold {TARGET_FPS}fps");
    println!("  queue_full   -- dropped because this consumer was too slow");
    println!("  unusable     -- dropped because the buffer could not be decoded");
    println!();
    println!("If offered is near {expected:.0} and rate_limited is most of the gap, the");
    println!("camera is fine and the limiter is doing its job. If offered is far below it,");
    println!("the camera does not sustain {TARGET_FPS}fps and no fix on our side changes that.");
}
