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

impl AudioPipeline {
    pub fn new() -> Self {
        Self {
            stop_tx: None,
            active: false,
        }
    }

    /// Start the capture pipeline: mic → Opus → peer connection.
    ///
    /// `io_cmd_tx` is the channel to send encoded audio to the I/O loop.
    pub fn start_capture(
        &mut self,
        io_cmd_tx: mpsc::Sender<IoCommand>,
    ) -> Result<(), String> {
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
            let opus_rate = match sample_rate {
                8000 | 12000 | 16000 | 24000 | 48000 => sample_rate,
                44100 => 48000, // Common case: resample 44.1k → 48k
                _ => 48000,
            };

            let mut encoder = match OpusEncoder::new(opus_rate, channels.min(2)) {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!("Failed to create Opus encoder: {e}");
                    return;
                }
            };

            tracing::info!(
                sample_rate, channels, opus_rate,
                "Audio capture started"
            );

            // Opus frame size: 20ms at the given sample rate
            let frame_samples = (opus_rate as usize * 20) / 1000;
            let frame_total_samples = frame_samples * channels.min(2) as usize;
            let mut accumulator: Vec<f32> = Vec::with_capacity(frame_total_samples * 2);

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
                        data = resample_44100_to_48000(&data, channels as usize);
                    }

                    accumulator.extend_from_slice(&data);

                    // Process complete Opus frames
                    while accumulator.len() >= frame_total_samples {
                        let frame_data: Vec<f32> =
                            accumulator.drain(..frame_total_samples).collect();

                        let audio_frame = AudioFrame {
                            sample_rate: opus_rate,
                            channels: channels.min(2),
                            data: frame_data,
                            timestamp_us: 0,
                        };

                        match encoder.encode(&audio_frame) {
                            Ok(encoded) => {
                                let _ = io_cmd_tx.try_send(IoCommand::WriteAudio(encoded));
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
    pub fn start_playback(
        event_rx: Arc<Mutex<mpsc::Receiver<PcEvent>>>,
    ) -> Result<(), String> {
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

            tracing::info!(
                play_rate, play_channels,
                "Audio playback started"
            );

            loop {
                let event = {
                    let mut rx = match event_rx.lock() {
                        Ok(rx) => rx,
                        Err(_) => return,
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
                                        decoded_frame.channels as usize,
                                        play_rate,
                                    );
                                    decoded_frame.sample_rate = play_rate;
                                }

                                // Adjust channel count if needed
                                if play_channels != decoded_frame.channels {
                                    decoded_frame.data = adjust_channels(
                                        &decoded_frame.data,
                                        decoded_frame.channels as usize,
                                        play_channels as usize,
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

    pub fn is_active(&self) -> bool {
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
    let ratio = 48000.0 / 44100.0;
    let input_frames = samples.len() / channels;
    let output_frames = (input_frames as f64 * ratio) as usize;
    let mut output = Vec::with_capacity(output_frames * channels);

    for i in 0..output_frames {
        let src_pos = i as f64 / ratio;
        let src_idx = src_pos as usize;
        let frac = (src_pos - src_idx as f64) as f32;

        for ch in 0..channels {
            let s0 = samples.get(src_idx * channels + ch).copied().unwrap_or(0.0);
            let s1 = samples
                .get((src_idx + 1) * channels + ch)
                .copied()
                .unwrap_or(s0);
            output.push(s0 + (s1 - s0) * frac);
        }
    }

    output
}

/// Simple resampling from 48000 Hz to a target rate.
fn resample_48000_to_target(samples: &[f32], channels: usize, target_rate: u32) -> Vec<f32> {
    let ratio = target_rate as f64 / 48000.0;
    let input_frames = samples.len() / channels;
    let output_frames = (input_frames as f64 * ratio) as usize;
    let mut output = Vec::with_capacity(output_frames * channels);

    for i in 0..output_frames {
        let src_pos = i as f64 / ratio;
        let src_idx = src_pos as usize;
        let frac = (src_pos - src_idx as f64) as f32;

        for ch in 0..channels {
            let s0 = samples.get(src_idx * channels + ch).copied().unwrap_or(0.0);
            let s1 = samples
                .get((src_idx + 1) * channels + ch)
                .copied()
                .unwrap_or(s0);
            output.push(s0 + (s1 - s0) * frac);
        }
    }

    output
}

/// Adjust the number of channels (mono↔stereo).
fn adjust_channels(samples: &[f32], from_ch: usize, to_ch: usize) -> Vec<f32> {
    if from_ch == to_ch {
        return samples.to_vec();
    }

    let frames = samples.len() / from_ch;
    let mut output = Vec::with_capacity(frames * to_ch);

    if from_ch == 1 && to_ch == 2 {
        // Mono → Stereo: duplicate
        for &s in samples {
            output.push(s);
            output.push(s);
        }
    } else if from_ch == 2 && to_ch == 1 {
        // Stereo → Mono: average
        for frame in samples.chunks(2) {
            let avg = (frame[0] + frame.get(1).copied().unwrap_or(0.0)) * 0.5;
            output.push(avg);
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
