//! Video pipeline: camera/screen → encode → str0m → decode → frame buffer
//!
//! This module wires together:
//! - Camera capture (via elementium-media) or screen capture frames
//! - VP8 encoding of captured frames
//! - Feeding encoded video into str0m peer connections
//! - Receiving encoded video from str0m
//! - VP8 decoding of received video
//! - Writing decoded frames to a shared buffer for the webview

use std::collections::HashMap;

use tokio::sync::mpsc;

use elementium_codec::{NegotiatedDecoder, VideoCodec, VideoDecoder};

use crate::engine::VideoFrameBuffer;
use crate::peer_connection::PcEvent;

/// Manages the video pipeline for a call session.
pub struct VideoPipeline {
    /// Whether playback (decode) is active.
    playback_active: bool,
}

impl VideoPipeline {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            playback_active: false,
        }
    }

    /// Start the playback (decode) pipeline: peer connection → VP8 decode → frame buffer.
    ///
    /// Decode failures inside the spawned thread are logged and skipped per-frame (see
    /// the `tracing::error!`/`tracing::debug!` calls below), never returned: the thread
    /// runs independently of this call and outlives it. A `Result` that could never be
    /// `Err` taught every caller to write dead error-handling around it -- see
    /// `specs/BACKLOG-2026-08-09-errors.md` X4.
    /// Returns the decode thread's handle, or `None` if playback was already running.
    /// Production callers drop it (which detaches, as before); it exists so a test can
    /// join the thread and prove it really does end when its channel closes, rather than
    /// asserting on the absence of something.
    pub fn start_playback(
        &mut self,
        mut event_rx: mpsc::Receiver<PcEvent>,
        frame_buffer: VideoFrameBuffer,
        pc_id: String,
    ) -> Option<std::thread::JoinHandle<()>> {
        if self.playback_active {
            return None;
        }
        self.playback_active = true;

        Some(std::thread::spawn(move || {
            // One decoder per remote track (`mid`), not one shared decoder for the whole
            // PC -- a single PeerConnection can carry more than one remote video track
            // (e.g. camera + screen share both active), and VP8 decoding is stateful
            // (keyframe/interframe references), so interleaving two tracks' packets
            // through one decoder would corrupt both. Same class of bug as the audio
            // pipeline's per-mid Opus decoder fix.
            // Keyed by mid *and* codec. A remote track that switches codec mid-call gets a
            // fresh decoder rather than feeding H.264 to a VP8 decoder, which produces
            // nothing and says nothing.
            let mut decoders: HashMap<(String, VideoCodec), NegotiatedDecoder> = HashMap::new();
            // (frames decoded, packets that produced nothing), per track.
            //
            // Inbound audio has been counted for months and inbound video was not, so
            // between "packets are arriving" and "there is a picture" the log said nothing
            // at all -- and "we decode nothing" and "we decode fine and draw nowhere" are
            // two different faults that both happened, on the same evening, looking
            // identical from outside.
            let mut tallies: HashMap<String, (u64, u64)> = HashMap::new();

            tracing::info!(pc_id = %pc_id, "Video playback pipeline started");

            // Blocks until a packet arrives rather than polling and sleeping -- see the
            // same change in `audio_pipeline`. Decoding is the expensive half of this
            // thread's work; the old shape spent the cheap half waking up to find an empty
            // queue, and could not tell an empty queue from a closed one, so the thread
            // outlived its connection forever.
            while let Some(event) = event_rx.blocking_recv() {
                // Other events (audio, state changes) are not routed here.
                let PcEvent::VideoData {
                    mid,
                    data: packet,
                    codec,
                } = event
                else {
                    continue;
                };
                {
                    let decoder = match decoders.entry((mid.clone(), codec)) {
                        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                        std::collections::hash_map::Entry::Vacant(v) => {
                            match NegotiatedDecoder::new(codec) {
                                Ok(d) => v.insert(d),
                                Err(e) => {
                                    tracing::error!(mid, ?codec, error = %e, "Failed to create decoder for track");
                                    continue;
                                }
                            }
                        }
                    };

                    // One display slot per track, not per connection.
                    //
                    // On an SFU every remote participant's video arrives on the same
                    // subscriber connection, told apart only by mid. Keyed by connection,
                    // two people on camera write to one slot sixty times a second and
                    // overwrite each other -- the viewer gets one flickering picture of
                    // two people, and no way to show the second at all. The decoders were
                    // already split per mid; only the display key was not.
                    let track_key = format!("{pc_id}-{mid}");
                    let tally = tallies.entry(mid.clone()).or_insert((0, 0));
                    match VideoDecoder::decode(decoder, &packet) {
                        Ok(frames) => {
                            for i420_frame in frames {
                                tally.0 = tally.0.saturating_add(1);
                                // Convert I420 to RGBA for display
                                let rgba_frame = elementium_codec::i420_to_rgba(&i420_frame);

                                // Store in the shared frame buffer.
                                if let Ok(mut buf) = frame_buffer.lock() {
                                    if let Some(existing) = buf.get_mut(&track_key) {
                                        *existing = rgba_frame;
                                    } else {
                                        buf.insert(track_key.clone(), rgba_frame);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tally.1 = tally.1.saturating_add(1);
                            tracing::debug!(mid, ?codec, "video decode error: {e}");
                        }
                    }
                    // Every 30th frame is about once a second on a 30fps track: often enough
                    // to measure a rate from, rare enough not to bury the rest of the log.
                    let report = (tally.0 > 0 && tally.0.is_multiple_of(30))
                        || (tally.1 > 0 && tally.1.is_multiple_of(100));
                    if report {
                        tracing::info!(
                            mid,
                            ?codec,
                            frames_decoded = tally.0,
                            decode_failures = tally.1,
                            "Inbound video frame decoded"
                        );
                    }
                }
            }

            tracing::info!(pc_id = %pc_id, "Video playback stopped: the event channel closed");
        }))
    }

    #[must_use]
    pub const fn is_playback_active(&self) -> bool {
        self.playback_active
    }
}

impl Default for VideoPipeline {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{VideoFrameBuffer, VideoPipeline};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// The decode thread must end when its sender is dropped.
    ///
    /// Fails against the previous implementation, which polled with `try_recv().ok()` --
    /// a closed channel and an empty one both yielded `None`, so the thread slept 5ms and
    /// looked again, forever. One leaked per peer connection, and a call that reconnects
    /// makes several.
    #[test]
    #[allow(clippy::expect_used)]
    fn the_decode_thread_ends_when_its_channel_closes() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let frames: VideoFrameBuffer = Arc::new(Mutex::new(HashMap::new()));
        let mut pipeline = VideoPipeline::new();
        let handle = pipeline
            .start_playback(rx, frames, "pc-test".to_owned())
            .expect("a fresh pipeline starts playback");

        drop(tx);

        // Joining directly would hang rather than fail if the thread never ends, which
        // reports a bug as a stuck test suite. Polling gives it a verdict.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle.is_finished() {
            assert!(
                Instant::now() < deadline,
                "the decode thread was still running 5s after its channel closed"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        handle.join().expect("the decode thread ended cleanly");
    }
}
