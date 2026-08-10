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

/// Ask the `XDG` desktop portal for a screencast `PipeWire` node id.
///
/// The same call `elementium-screen`'s Wayland capturer makes (see
/// `crates/elementium-screen/src/wayland.rs`), duplicated here rather than depended on:
/// `elementium-screen` depends on `elementium-media`, so pulling it in from this side would
/// be circular. Shows a picker dialog and blocks until a person chooses -- deliberately
/// unbounded, since a timeout would cancel a dialog someone was still reading.
#[cfg(target_os = "linux")]
async fn request_screencast_node() -> Result<u32, String> {
    use ashpd::desktop::screencast::{
        CursorMode, Screencast, SelectSourcesOptions, SourceType, StartCastOptions,
    };
    use ashpd::desktop::CreateSessionOptions;

    let proxy = Screencast::new().await.map_err(|e| e.to_string())?;
    let session = proxy
        .create_session(CreateSessionOptions::default())
        .await
        .map_err(|e| e.to_string())?;

    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(SourceType::Monitor | SourceType::Window)
                .set_multiple(false)
                .set_persist_mode(ashpd::desktop::PersistMode::DoNot),
        )
        .await
        .map_err(|e| e.to_string())?;

    let response = proxy
        .start(&session, None, StartCastOptions::default())
        .await
        .map_err(|e| e.to_string())?
        .response()
        .map_err(|e| e.to_string())?;

    response
        .streams()
        .first()
        .map(ashpd::desktop::screencast::Stream::pipe_wire_node_id)
        .ok_or_else(|| "the portal returned no streams; the picker was probably cancelled".to_owned())
}

#[cfg(target_os = "linux")]
#[allow(clippy::print_stdout)]
/// Block on the portal exchange and open the granted node as a `VideoSource`.
///
/// Synchronous on purpose, like the rest of this example: a short-lived runtime runs just
/// the async portal call, matching how `WaylandCapturer::start` does it, so the measurement
/// loop below never has to know `async` was involved.
fn open_screencast_source(
    target: elementium_codec::EncodeTarget,
) -> Result<elementium_media::video_source::VideoSource, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    let node_id = runtime.block_on(request_screencast_node())?;
    println!("portal granted node {node_id}; opening it directly");
    elementium_media::video_source::VideoSource::start_screencast(node_id, target)
}

/// Counters produced by running the measurement loop for [`SECONDS`].
struct Measurement {
    received: u64,
    span: f64,
    cpu_used: f64,
}

/// Write the first frame's luma plane to `$CAPTURE_DUMP` as a binary PGM, if that is set.
///
/// PGM because it is eight lines of code with no image dependency, and every viewer and
/// `feh`/`display`/GIMP opens it. Off unless the variable is set: this example's normal job
/// is timing, and writing eleven megabytes per run would distort the thing it measures.
#[allow(clippy::print_stdout)]
fn dump_first_frame(frame: &elementium_media::captured_frame::CapturedFrame) {
    let Ok(path) = std::env::var("CAPTURE_DUMP") else {
        return;
    };
    dump_to(frame, &path);
}

/// Write one frame's luma plane to `path` as a binary PGM.
#[allow(clippy::print_stdout)]
fn dump_to(frame: &elementium_media::captured_frame::CapturedFrame, path: &str) {
    let elementium_media::captured_frame::CapturedFrame::Planar(planar) = frame else {
        println!("  (frame is undecoded JPEG; not dumping)");
        return;
    };
    let mut out = format!("P5\n{} {}\n255\n", planar.width(), planar.height()).into_bytes();
    out.extend_from_slice(planar.y());
    match std::fs::write(path, &out) {
        Ok(()) => println!("  wrote the first frame's luma plane to {path}"),
        Err(e) => println!("  could not write {path}: {e}"),
    }
}

/// Mean luma over a sample of the frame, or 0 for a frame that carries no pixels.
///
/// Sampled rather than exhaustive: this runs on every frame inside the receive loop, and a
/// full pass over a 7-megapixel plane would add latency to the measurement of latency.
#[allow(clippy::arithmetic_side_effects, clippy::cast_precision_loss)]
fn mean_luma(frame: &elementium_media::captured_frame::CapturedFrame) -> f64 {
    let elementium_media::captured_frame::CapturedFrame::Planar(planar) = frame else {
        return 0.0;
    };
    let y = planar.y();
    let step = (y.len() / 20_000).max(1);
    let mut total: u64 = 0;
    let mut count: u64 = 0;
    for i in (0..y.len()).step_by(step) {
        total += u64::from(y.get(i).copied().unwrap_or(0));
        count += 1;
    }
    if count == 0 {
        return 0.0;
    }
    // `u32::try_from(..).map(f64::from)` rather than `as`: the workspace bans silent
    // numeric casts, and a sample count that overflowed a u32 would be a bug worth seeing.
    let sum = u32::try_from(total.min(u64::from(u32::MAX))).map_or(0.0, f64::from);
    let n = u32::try_from(count.min(u64::from(u32::MAX))).map_or(1.0, f64::from);
    sum / n
}

#[allow(clippy::arithmetic_side_effects)]
/// Run the same receive loop the camera path uses, against anything that hands back frames.
///
/// Pulled out so `--screen` and the camera paths measure identically -- the point of this
/// example is comparing the two, which is only valid if both are timed the same way.
fn measure(try_recv: impl Fn() -> Option<elementium_media::captured_frame::CapturedFrame>) -> Measurement {
    let cpu_before = cpu_seconds();
    let deadline = Instant::now().checked_add(Duration::from_secs(seconds())).unwrap_or_else(Instant::now);
    let mut received = 0_u64;
    let started = Instant::now();
    let mut last_slot = u64::MAX;
    let latency_probe = std::env::var("CAPTURE_LATENCY").is_ok();
    let mut baseline_luma: Option<f64> = None;
    let mut first_at: Option<Instant> = None;
    let mut last_at: Option<Instant> = None;
    while Instant::now() < deadline {
        if let Some(frame) = try_recv() {
            if latency_probe {
                // Report the wall-clock moment the captured picture first moves.
                //
                // SC-002 asks how long a change on screen takes to reach a viewer. The half
                // that is ours -- compositor damage, PipeWire delivery, our decode and
                // queueing -- is measurable here: a driver notes when it changed the shared
                // window, this notes when that change arrived, and the difference is the
                // capture-side latency. The mean is taken over a sample of the luma plane
                // rather than every byte, because this runs inside the receive loop and
                // must not become the thing it is measuring.
                let mean = mean_luma(&frame);
                match baseline_luma {
                    None => baseline_luma = Some(mean),
                    Some(base) if (mean - base).abs() > 2.0 => {
                        let at = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_millis());
                        println!("PICTURE_CHANGED_AT {at} (mean luma {base:.1} -> {mean:.1})");
                        baseline_luma = Some(mean);
                    }
                    Some(_) => {}
                }
            }
            if let Ok(prefix) = std::env::var("CAPTURE_DUMP_SERIES") {
                // A numbered series rather than one frame, so a run can be compared against
                // itself over time: share one window, change something else, and see
                // whether the captured picture moved. That is SC-003's actual claim, and no
                // counter can answer it.
                let slot = Instant::now().saturating_duration_since(started).as_secs() / 5;
                if slot != last_slot {
                    last_slot = slot;
                    dump_to(&frame, &format!("{prefix}{slot:02}.pgm"));
                }
            }
            if received == 0 {
                // The first frame, written out as a greyscale image when asked for.
                //
                // Counters cannot tell a real picture from well-shaped noise, and the
                // difference matters most on the paths where it is easiest to get wrong:
                // a DMA-BUF read with the wrong stride, or a tiled buffer read as if it
                // were linear, both produce frames at the right rate and the right size,
                // full of garbage. This feature already has one such fault in its history
                // -- frames counted, published, and decodable by nobody. Looking is the
                // only check that catches it.
                dump_first_frame(&frame);
            }
            received = received.saturating_add(1);
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
    Measurement { received, span, cpu_used: cpu_seconds() - cpu_before }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::as_conversions,
    clippy::print_stdout,
    clippy::arithmetic_side_effects
)]
/// Print the counters shared by every capture path, so screen and camera runs read the same.
fn report(m: &Measurement) {
    let delivered_fps = if m.span > 0.0 {
        (m.received.saturating_sub(1)) as f64 / m.span
    } else {
        0.0
    };
    let expected = f64::from(TARGET_FPS) * m.span;

    println!();
    println!("  frames the consumer received: {}", m.received);
    println!("  process CPU during capture: {:.2}s", m.cpu_used);
    if m.received > 0 {
        let per_frame_ms = m.cpu_used * 1000.0 / m.received as f64;
        println!("  process CPU per frame: {per_frame_ms:.2}ms");
    }
    println!("  over: {:.1}s", m.span);
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
            elementium_media::pipewire_capture::DEFAULT_CAPTURE_SIZE,
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

/// Override the measurement length, for runs that orchestrate something around the capture
/// (see `CAPTURE_DUMP_SERIES`, which needs time for a change to be made and observed).
fn seconds() -> u64 {
    std::env::var("CAPTURE_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(SECONDS)
}

#[allow(clippy::print_stdout)]
/// Enumerate cameras, pick a delivering one, and measure it.
///
/// Split out of `main` so the camera and `--screen` paths are two short, parallel branches
/// rather than one function that decides half-way through which source it is measuring.
fn run_camera(target: elementium_codec::EncodeTarget) {
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
    let m = measure(|| capturer.try_recv());
    report(&m);
}

#[cfg(target_os = "linux")]
#[allow(clippy::print_stdout)]
/// Ask the portal for a screencast node, open it, and measure it the same way.
/// Open and close the granted screencast node `cycles` times, then hold, so the `PipeWire`
/// graph can be inspected from outside for what the teardown left behind (SC-006).
///
/// One portal grant, many capture cycles, because the leak being measured is in *our*
/// teardown -- the stream, its thread and its node -- and asking a person to click a picker
/// ten times measures their patience instead. The portal session's own teardown is a
/// separate question, answered by `ShareSession::close` and its `Drop` backstop.
#[cfg(target_os = "linux")]
fn run_screen_cycles(target: elementium_codec::EncodeTarget, cycles: u32) {
    let node_id = {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
            println!("could not build a runtime for the portal call");
            return;
        };
        match runtime.block_on(request_screencast_node()) {
            Ok(id) => id,
            Err(e) => {
                println!("portal: {e}");
                return;
            }
        }
    };
    println!("portal granted node {node_id}; running {cycles} open/close cycles");

    for i in 1..=cycles {
        match elementium_media::video_source::VideoSource::start_screencast(node_id, target) {
            Ok(source) => {
                std::thread::sleep(Duration::from_millis(800));
                let (w, h) = source.size();
                source.stop();
                drop(source);
                println!("  cycle {i}: opened {w}x{h}, stopped");
            }
            Err(e) => println!("  cycle {i}: could not open: {e}"),
        }
    }

    // Held open deliberately: the thing being measured is what survives *inside a running
    // process*, and process exit would clean up a leak rather than reveal it.
    println!("all cycles done; holding for 20s -- inspect now, e.g.:");
    println!("  pw-dump | grep -c elementium-capture");
    std::thread::sleep(Duration::from_secs(20));
}

fn run_screen(target: elementium_codec::EncodeTarget) {
    let source = match open_screencast_source(target) {
        Ok(source) => source,
        Err(e) => {
            println!("could not start screencast capture: {e}");
            return;
        }
    };
    let m = measure(|| source.try_recv());
    report(&m);
    // Whether the source *died* during the run, as opposed to simply having had nothing to
    // send. A shared window that gets closed mid-share errors the stream, after which
    // `try_recv` returns `None` forever -- which is exactly what a healthy screencast of a
    // static window looks like. Printing the distinction here is how that can be checked
    // by hand: start a share of a window, close the window, and this must say `true`.
    println!("  source failed during the run: {}", source.failed());
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::print_stdout)]
/// The portal this relies on is Linux-only (`org.freedesktop.portal.ScreenCast`).
fn run_screen(_target: elementium_codec::EncodeTarget) {
    println!("--screen needs the XDG desktop portal, which this platform does not have");
}

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
    let args: Vec<String> = std::env::args().collect();
    let hardware = args.iter().any(|a| a == "--hardware");
    let screen = args.iter().any(|a| a == "--screen");
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

    let cycles: u32 = args
        .iter()
        .position(|a| a == "--cycles")
        .and_then(|i| args.get(i.saturating_add(1)))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    if screen && cycles > 0 {
        #[cfg(target_os = "linux")]
        run_screen_cycles(target, cycles);
        #[cfg(not(target_os = "linux"))]
        println!("--cycles needs the XDG desktop portal, which this platform does not have");
    } else if screen {
        run_screen(target);
    } else {
        run_camera(target);
    }
}
