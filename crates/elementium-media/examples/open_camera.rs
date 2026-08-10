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
//! ```
//!
//! Captures a handful of frames and reports their size, then stops. Nothing is written to
//! disk: this says whether a camera produces frames, not what is in them.

use std::time::{Duration, Instant};

/// How long to watch for frames once a source has been opened.
const WATCH: Duration = Duration::from_secs(3);

fn main() {
    let preferred: Option<u32> = std::env::args().nth(1).and_then(|a| a.parse().ok());
    println!("preferred node: {}", preferred.map_or_else(|| "none".to_owned(), |n| n.to_string()));

    let started = Instant::now();
    let source = match elementium_media::video_source::VideoSource::start_at_device(
        None,
        None,
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
