//! Open a camera the way the application does, and say what happened.
//!
//! Same call, same fallback order, same first-frame wait as a real call's camera pipeline --
//! but in twenty seconds and without a call, a room, a login or a webview around it. Written
//! after a run spent twelve minutes reporting "never sent encoded video" for what was, in the
//! end, a question about which camera got opened.
//!
//! Takes an optional preferred `PipeWire` node id, the same way a page's `deviceId` does:
//!
//! ```text
//! nix develop -c cargo run -p elementium-media --example open_camera        # no preference
//! nix develop -c cargo run -p elementium-media --example open_camera 349    # ask for one
//! nix develop -c cargo run -p elementium-media --example open_camera 349 1920 1080
//! ```
//!
//! Captures a handful of frames and reports their size, then stops. Nothing is written to
//! disk: this says whether a camera produces frames, not what is in them.

use std::time::{Duration, Instant};

/// How long to watch for frames once a source has been opened.
const WATCH: Duration = Duration::from_secs(3);

/// One optional numeric argument, or a refusal naming it.
///
/// `.ok()` here would turn a typo into "no preference": ask for node `394` instead of `349`
/// and the run would open some other camera and print a confident report about it. A
/// diagnostic tool that quietly answers a different question than the one asked is worse than
/// one that refuses.
fn numeric_arg(position: usize, name: &str) -> Result<Option<u32>, String> {
    let Some(raw) = std::env::args().nth(position) else {
        return Ok(None);
    };
    raw.parse()
        .map(Some)
        .map_err(|e| format!("{name} argument {raw:?} is not a number: {e}"))
}

fn main() {
    let (preferred, width, height) = match (
        numeric_arg(1, "node id"),
        numeric_arg(2, "width"),
        numeric_arg(3, "height"),
    ) {
        (Ok(node), Ok(w), Ok(h)) => (node, w, h),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
            eprintln!("{e}");
            return;
        }
    };
    println!("preferred node: {}", preferred.map_or_else(|| "none".to_owned(), |n| n.to_string()));

    // Size is separate from the node id because "which camera" and "how big" are different
    // questions and this example has been used to answer each without the other.
    println!(
        "requested size: {}",
        match (width, height) {
            (Some(w), Some(h)) => format!("{w}x{h}"),
            _ => "none (the negotiation's own default)".to_owned(),
        }
    );

    let started = Instant::now();
    let source = match elementium_media::video_source::VideoSource::start_at_device(
        width,
        height,
        30,
        elementium_codec::EncodeTarget::software(),
        preferred,
    ) {
        Ok(s) => s,
        Err(e) => {
            println!("no camera started after {:.1}s: {e}", started.elapsed().as_secs_f64());
            return;
        }
    };

    let (width, height) = source.size();
    println!(
        "opened via {} in {:.1}s, reporting {width}x{height}",
        source.backend(),
        started.elapsed().as_secs_f64(),
    );

    let watch_start = Instant::now();
    let mut frames = 0_u32;
    let mut last = (0, 0);
    while watch_start.elapsed() < WATCH {
        if let Some(frame) = source.try_recv() {
            frames = frames.saturating_add(1);
            last = (frame.width(), frame.height());
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    source.stop();

    let seconds = watch_start.elapsed().as_secs_f64();
    println!("{frames} frames in {seconds:.1}s ({:.1}fps), last {}x{}", f64::from(frames) / seconds, last.0, last.1);
}
