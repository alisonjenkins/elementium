//! Audio pipeline: capture → encode → str0m → decode → playback
//!
//! This module wires together:
//! - cpal audio capture (microphone)
//! - Opus encoding of captured audio
//! - Feeding encoded audio into str0m peer connections
//! - Receiving encoded audio from str0m
//! - Opus decoding of received audio
//! - cpal audio playback (speakers)

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use elementium_codec::{OpusDecoder, OpusEncoder};
use elementium_media::audio_capture::AudioCapturer;
use elementium_media::audio_playback::AudioPlayer;
use elementium_types::AudioFrame;

use crate::engine::IoCommand;
use crate::peer_connection::PcEvent;

/// Manages the audio pipeline for a call session.
pub struct AudioPipeline {
    /// Channel to stop the capture loop.
    stop_tx: Option<mpsc::Sender<()>>,
    /// Whether the pipeline is currently active.
    active: bool,
}

impl Default for AudioPipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioPipeline {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            stop_tx: None,
            active: false,
        }
    }

    /// Start the capture pipeline: mic → Opus → peer connection.
    ///
    /// `io_cmd_tx` is the channel to send encoded audio to the I/O loop.
    ///
    /// # Errors
    ///
    /// This function currently always succeeds; failures inside the spawned
    /// capture thread (device or encoder setup) are logged rather than
    /// propagated, since the thread runs independently of the caller.
    pub fn start_capture(&mut self, io_cmd_tx: mpsc::Sender<IoCommand>) -> Result<(), String> {
        if self.active {
            return Ok(());
        }

        let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        self.stop_tx = Some(stop_tx);
        self.active = true;

        // Start the capture in a blocking thread
        std::thread::spawn(move || {
            let capturer: AudioCapturer = match AudioCapturer::start() {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("Failed to start audio capture: {e}");
                    return;
                }
            };

            let sample_rate = capturer.sample_rate();
            let channels = capturer.channels();

            // Opus needs 48kHz. If capture rate differs, we'll need resampling.
            // For now, create encoder at the capture rate (Opus supports 8/12/16/24/48kHz).
            // Anything other than a native Opus rate (including the common
            // 44.1kHz capture rate) gets resampled to 48kHz.
            let opus_rate = match sample_rate {
                8000 | 12000 | 16000 | 24000 | 48000 => sample_rate,
                _ => 48000,
            };

            let encode_channels = channels.min(2);

            let mut encoder = match OpusEncoder::new(opus_rate, encode_channels) {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!("Failed to create Opus encoder: {e}");
                    return;
                }
            };

            tracing::info!(sample_rate, channels, opus_rate, "Audio capture started");

            // Opus frame size: 20ms at the given sample rate
            let Some(frame_samples) = usize::try_from(opus_rate)
                .ok()
                .and_then(|r| r.checked_mul(20))
                .and_then(|v| v.checked_div(1000))
            else {
                tracing::error!("Failed to compute Opus frame size");
                return;
            };
            let Some(frame_total_samples) =
                frame_samples.checked_mul(usize::from(encode_channels))
            else {
                tracing::error!("Frame size calculation overflowed");
                return;
            };
            let Some(accumulator_capacity) = frame_total_samples.checked_mul(2) else {
                tracing::error!("Accumulator capacity calculation overflowed");
                return;
            };
            let mut accumulator: Vec<f32> = Vec::with_capacity(accumulator_capacity);

            loop {
                // Check for stop signal (non-blocking)
                if stop_rx.try_recv().is_ok() {
                    tracing::info!("Audio capture stopping");
                    break;
                }

                // Get audio data from the microphone
                if let Some(frame) = capturer.try_recv() {
                    let mut data = frame.data;

                    // Simple sample rate conversion for 44.1kHz → 48kHz
                    if sample_rate == 44100 && opus_rate == 48000 {
                        data = resample_44100_to_48000(&data, usize::from(channels));
                    }

                    accumulator.extend_from_slice(&data);

                    // Process complete Opus frames
                    while accumulator.len() >= frame_total_samples {
                        let frame_data: Vec<f32> =
                            accumulator.drain(..frame_total_samples).collect();

                        let audio_frame = AudioFrame {
                            sample_rate: opus_rate,
                            channels: encode_channels,
                            data: frame_data,
                            timestamp_us: 0,
                        };

                        match encoder.encode(&audio_frame) {
                            Ok(opus_packet) => {
                                let _ = io_cmd_tx.try_send(IoCommand::WriteAudio(opus_packet));
                            }
                            Err(e) => {
                                tracing::debug!("Opus encode error: {e}");
                            }
                        }
                    }
                } else {
                    // No audio available, sleep briefly
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        });

        Ok(())
    }

    /// Start the playback pipeline: peer connection → Opus decode → speakers.
    ///
    /// `event_rx` provides audio data events from the I/O loop.
    ///
    /// # Errors
    ///
    /// This function currently always succeeds; failures inside the spawned
    /// playback thread (device or decoder setup) are logged rather than
    /// propagated, since the thread runs independently of the caller.
    pub fn start_playback(event_rx: Arc<Mutex<mpsc::Receiver<PcEvent>>>) -> Result<(), String> {
        std::thread::spawn(move || {
            let player: AudioPlayer = match AudioPlayer::start() {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!("Failed to start audio playback: {e}");
                    return;
                }
            };

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
                            Ok(mut decoded_frame) => {
                                // Adjust sample rate if needed
                                if play_rate != 48000 {
                                    decoded_frame.data = resample_48000_to_target(
                                        &decoded_frame.data,
                                        usize::from(decoded_frame.channels),
                                        play_rate,
                                    );
                                    decoded_frame.sample_rate = play_rate;
                                }

                                // Adjust channel count if needed
                                if play_channels != decoded_frame.channels {
                                    decoded_frame.data = adjust_channels(
                                        &decoded_frame.data,
                                        usize::from(decoded_frame.channels),
                                        usize::from(play_channels),
                                    );
                                    decoded_frame.channels = play_channels;
                                }

                                player.play(decoded_frame);
                            }
                            Err(e) => {
                                tracing::debug!("Opus decode error: {e}");
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

    /// Stop the capture pipeline.
    pub fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.try_send(());
        }
        self.active = false;
    }

    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.active
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Simple linear interpolation resampling from 44100 to 48000 Hz.
fn resample_44100_to_48000(samples: &[f32], channels: usize) -> Vec<f32> {
    resample_linear(samples, channels, 48000.0 / 44100.0)
}

/// Simple resampling from 48000 Hz to a target rate.
fn resample_48000_to_target(samples: &[f32], channels: usize, target_rate: u32) -> Vec<f32> {
    resample_linear(samples, channels, f64::from(target_rate) / 48000.0)
}

/// Shared linear-interpolation resampling core used by the 44.1k↔48k helpers.
///
/// Sample counts here are bounded by audio frame sizes (tens of milliseconds
/// of PCM), far below `usize`/`f64` precision limits, so the lossy
/// float/index conversions used for interpolation are not a practical
/// correctness concern.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::as_conversions
)]
fn resample_linear(samples: &[f32], channels: usize, ratio: f64) -> Vec<f32> {
    if channels == 0 {
        return Vec::new();
    }

    let Some(input_frames) = samples.len().checked_div(channels) else {
        return Vec::new();
    };
    let output_frames = (input_frames as f64 * ratio) as usize;
    let Some(output_capacity) = output_frames.checked_mul(channels) else {
        return Vec::new();
    };
    let mut output = Vec::with_capacity(output_capacity);

    for i in 0..output_frames {
        let src_pos = i as f64 / ratio;
        let src_idx = src_pos as usize;
        let frac = (src_pos - src_idx as f64) as f32;

        for ch in 0..channels {
            let s0 = src_idx
                .checked_mul(channels)
                .and_then(|v| v.checked_add(ch))
                .and_then(|idx| samples.get(idx))
                .copied()
                .unwrap_or(0.0);
            let s1 = src_idx
                .checked_add(1)
                .and_then(|v| v.checked_mul(channels))
                .and_then(|v| v.checked_add(ch))
                .and_then(|idx| samples.get(idx))
                .copied()
                .unwrap_or(s0);
            output.push((s1 - s0).mul_add(frac, s0));
        }
    }

    output
}

/// Adjust the number of channels (mono↔stereo).
fn adjust_channels(samples: &[f32], from_ch: usize, to_ch: usize) -> Vec<f32> {
    if from_ch == to_ch || from_ch == 0 {
        return samples.to_vec();
    }

    let Some(frames) = samples.len().checked_div(from_ch) else {
        return Vec::new();
    };
    let Some(output_capacity) = frames.checked_mul(to_ch) else {
        return Vec::new();
    };
    let mut output = Vec::with_capacity(output_capacity);

    if from_ch == 1 && to_ch == 2 {
        // Mono → Stereo: duplicate
        for &s in samples {
            output.push(s);
            output.push(s);
        }
    } else if from_ch == 2 && to_ch == 1 {
        // Stereo → Mono: average
        for frame in samples.chunks(2) {
            let first = frame.first().copied().unwrap_or(0.0);
            let second = frame.get(1).copied().unwrap_or(0.0);
            output.push((first + second) * 0.5);
        }
    } else {
        // Generic: take first `to_ch` channels or zero-pad
        for frame in samples.chunks(from_ch) {
            for ch in 0..to_ch {
                output.push(frame.get(ch).copied().unwrap_or(0.0));
            }
        }
    }

    output
}
