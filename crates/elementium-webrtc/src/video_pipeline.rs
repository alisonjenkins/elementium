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
use std::sync::{Arc, Mutex};

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
    pub fn start_playback(
        &mut self,
        event_rx: Arc<Mutex<mpsc::Receiver<PcEvent>>>,
        frame_buffer: VideoFrameBuffer,
        pc_id: String,
    ) {
        if self.playback_active {
            return;
        }
        self.playback_active = true;

        std::thread::spawn(move || {
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

            tracing::info!(pc_id = %pc_id, "Video playback pipeline started");

            loop {
                let event = {
                    let Ok(mut rx) = event_rx.lock() else {
                        return;
                    };
                    rx.try_recv().ok()
                };

                match event {
                    Some(PcEvent::VideoData {
                        mid,
                        data: packet,
                        codec,
                    }) => {
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
                        match VideoDecoder::decode(decoder, &packet) {
                            Ok(frames) => {
                                for i420_frame in frames {
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
                                tracing::debug!(mid, ?codec, "video decode error: {e}");
                            }
                        }
                    }
                    Some(_) => {
                        // Other events (audio, state changes) are not handled here
                    }
                    None => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                }
            }
        });
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
