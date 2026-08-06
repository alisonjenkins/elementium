//! Audio playback pipeline: str0m → decode → speakers.
//!
//! The capture side (mic → Opus → peer connection) lives in
//! `src-tauri/src/commands/media_devices.rs`, which is the actual code path wired up to
//! `getUserMedia`; this module's own former `AudioPipeline::start_capture` was dead code
//! (never constructed anywhere) and has been removed rather than left to drift out of sync
//! with the real capture implementation.

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use elementium_codec::OpusDecoder;
use elementium_media::audio_playback::AudioPlayer;

use crate::peer_connection::PcEvent;

/// Start the playback pipeline: peer connection → Opus decode → speakers.
///
/// `event_rx` provides audio data events from the I/O loop.
///
/// # Errors
///
/// This function currently always succeeds; failures inside the spawned playback thread
/// (device or decoder setup) are logged rather than propagated, since the thread runs
/// independently of the caller.
pub fn start_playback(event_rx: Arc<Mutex<mpsc::Receiver<PcEvent>>>) -> Result<(), String> {
    std::thread::spawn(move || {
        let player: AudioPlayer = match AudioPlayer::start() {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("Failed to start audio playback: {e}");
                return;
            }
        };

        // AudioPlayer::play() resamples/remixes to the device's actual negotiated
        // rate/channels internally, so the decoder can stay fixed at Opus's native
        // 48kHz/stereo regardless of what the output device negotiates -- no manual
        // resample/remix step needed here.
        let play_rate = player.sample_rate();
        let play_channels = player.channels();

        let mut decoder = match OpusDecoder::new(48000, 2) {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("Failed to create Opus decoder: {e}");
                return;
            }
        };

        tracing::info!(play_rate, play_channels, "Audio playback started");
        let mut decoded_count: u64 = 0;

        loop {
            let event = {
                let Ok(mut rx) = event_rx.lock() else {
                    return;
                };
                rx.try_recv().ok()
            };

            match event {
                Some(PcEvent::AudioData(opus_packet)) => {
                    // Decode the Opus packet
                    // 20ms at 48kHz = 960 samples per channel
                    match decoder.decode(&opus_packet, 960) {
                        Ok(decoded_frame) => {
                            decoded_count = decoded_count.saturating_add(1);
                            if decoded_count.is_multiple_of(100) {
                                tracing::info!(
                                    count = decoded_count,
                                    opus_len = opus_packet.len(),
                                    decoded_samples = decoded_frame.data.len(),
                                    decoded_sample_rate = decoded_frame.sample_rate,
                                    decoded_channels = decoded_frame.channels,
                                    "Decoded inbound Opus audio frame"
                                );
                            }
                            player.play(decoded_frame);
                        }
                        Err(e) => {
                            tracing::warn!(
                                opus_len = opus_packet.len(),
                                error = %e,
                                "Failed to decode inbound Opus frame, dropping"
                            );
                        }
                    }
                }
                _ => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
    });

    Ok(())
}

