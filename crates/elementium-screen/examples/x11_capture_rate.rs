//! Measure how fast X11 capture actually runs, and where the time goes.
//!
//! M7 in `specs/BACKLOG-2026-08-09-media.md` recorded X11 share capture running at about
//! 3.3fps under Xvfb while the camera path held exactly 30. That number came out of a full
//! call test, which cannot say which part of a capture is slow -- and the obvious remedy
//! (the MIT-SHM extension) is a large change to make on the strength of a number nobody has
//! broken down.
//!
//! This breaks it down. It times the two halves separately:
//!
//! * *resolve* -- `xcap::Monitor::all()`/`Window::all()`, which asks the X server for every
//!   monitor or window and then finds the one wanted.
//! * *capture* -- `capture_image()`, which is xcap talking to the X server.
//! * *convert* -- BGRA to I420, which is our own pixel work on the same path.
//!
//! Nothing is written anywhere and no pixel ever leaves this process: it reports frame counts
//! and milliseconds only. Run it against a throwaway display rather than a desktop:
//!
//! ```text
//! nix shell nixpkgs#xvfb-run --command xvfb-run -a -s "-screen 0 1280x800x24" \
//!   nix develop -c cargo run -p elementium-screen --example x11_capture_rate
//! ```
//!
//! Takes the source id as an optional argument (`monitor-0`, `window-12345`); defaults to the
//! first monitor the enumeration offers.

use std::time::{Duration, Instant};

/// How many frames to time. Thirty is a second of video at the rate the capture loop targets,
/// and long enough that a single slow first call does not dominate the average.
const FRAMES: u32 = 30;

fn main() {
    let Some(source_id) = std::env::args().nth(1).or_else(first_monitor_id) else {
        eprintln!("no monitor could be enumerated: is there an X display to capture?");
        return;
    };

    println!("source: {source_id}");
    match elementium_screen::x11::source_size(&source_id) {
        Ok((w, h)) => println!("declared size: {w}x{h}"),
        Err(e) => println!("declared size: unavailable ({e})"),
    }

    let mut resolve_total = Duration::ZERO;
    let mut capture_total = Duration::ZERO;
    let mut convert_total = Duration::ZERO;
    let mut captured = 0_u32;
    let started = Instant::now();

    for _ in 0..FRAMES {
        let resolve_start = Instant::now();
        let monitors = xcap::Monitor::all().unwrap_or_default();
        let found = monitors.into_iter().find(|m| {
            m.id().is_ok_and(|id| source_id == format!("monitor-{id}"))
        });
        resolve_total = resolve_total.saturating_add(resolve_start.elapsed());

        let Some(monitor) = found else { continue };

        let capture_start = Instant::now();
        let image = monitor.capture_image();
        capture_total = capture_total.saturating_add(capture_start.elapsed());

        if let Ok(image) = image {
            // Timed apart from the capture: the conversion is on the per-frame path in the
            // real capturer, so leaving it out would flatter the number this exists to check
            // -- but folding it in would hide which of the two is actually the cost.
            let convert_start = Instant::now();
            let (width, height) = (image.width(), image.height());
            let frame = elementium_codec::bgra_to_i420(width, height, &image.into_raw());
            std::hint::black_box(&frame);
            convert_total = convert_total.saturating_add(convert_start.elapsed());
            captured = captured.saturating_add(1);
        }
    }

    let wall = started.elapsed();
    println!("captured {captured}/{FRAMES} frames in {:.1}ms", wall.as_secs_f64() * 1000.0);
    if captured > 0 {
        println!("effective rate: {:.2}fps", f64::from(captured) / wall.as_secs_f64());
    }
    println!(
        "  resolve (Monitor::all + find): {:.1}ms total, {:.1}ms per frame",
        resolve_total.as_secs_f64() * 1000.0,
        resolve_total.as_secs_f64() * 1000.0 / f64::from(FRAMES),
    );
    println!(
        "  capture (xcap capture_image): {:.1}ms total, {:.1}ms per frame",
        capture_total.as_secs_f64() * 1000.0,
        capture_total.as_secs_f64() * 1000.0 / f64::from(FRAMES),
    );
    println!(
        "  convert (BGRA to I420): {:.1}ms total, {:.1}ms per frame",
        convert_total.as_secs_f64() * 1000.0,
        convert_total.as_secs_f64() * 1000.0 / f64::from(FRAMES),
    );
}

/// The id of whichever monitor enumerates first, in the `monitor-<id>` form the capturer takes.
fn first_monitor_id() -> Option<String> {
    let monitors = xcap::Monitor::all().ok()?;
    monitors.into_iter().find_map(|m| m.id().ok().map(|id| format!("monitor-{id}")))
}
