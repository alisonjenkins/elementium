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

use elementium_codec::Vp8Decoder;

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
    /// # Errors
    ///
    /// This implementation currently always returns `Ok`, but the signature
    /// is fallible to allow future validation to reject the request.
    pub fn start_playback(
        &mut self,
        event_rx: Arc<Mutex<mpsc::Receiver<PcEvent>>>,
        frame_buffer: VideoFrameBuffer,
        pc_id: String,
    ) -> Result<(), String> {
        if self.playback_active {
            return Ok(());
        }
        self.playback_active = true;

        std::thread::spawn(move || {
            // One decoder per remote track (`mid`), not one shared decoder for the whole
            // PC -- a single PeerConnection can carry more than one remote video track
            // (e.g. camera + screen share both active), and VP8 decoding is stateful
            // (keyframe/interframe references), so interleaving two tracks' packets
            // through one decoder would corrupt both. Same class of bug as the audio
            // pipeline's per-mid Opus decoder fix.
            let mut decoders: HashMap<String, Vp8Decoder> = HashMap::new();

            tracing::info!(pc_id = %pc_id, "Video playback pipeline started");
            // The frontend looks this key up by pc_id alone (it doesn't know our
            // internal per-mid decoder split), so every track's decoded frames land in
            // the same display slot -- same external contract as before this fix,
            // which only changed decoder *state* isolation, not the display key.
            let track_key = format!("{pc_id}-video");

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
                        data: vp8_packet,
                    }) => {
                        let decoder = match decoders.entry(mid.clone()) {
                            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                            std::collections::hash_map::Entry::Vacant(v) => match Vp8Decoder::new()
                            {
                                Ok(d) => v.insert(d),
                                Err(e) => {
                                    tracing::error!(mid, error = %e, "Failed to create VP8 decoder for track");
                                    continue;
                                }
                            },
                        };

                        match decoder.decode(&vp8_packet) {
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
                                tracing::debug!(mid, "VP8 decode error: {e}");
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

        Ok(())
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
