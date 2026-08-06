use std::sync::mpsc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Stream;
use elementium_types::AudioFrame;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum PlaybackError {
    #[error("No output device available")]
    NoDevice,
    #[error("Failed to get device config: {0}")]
    Config(String),
    #[error("Failed to build stream: {0}")]
    Stream(String),
}

/// Plays audio to the default output device.
pub struct AudioPlayer {
    _stream: Stream,
    sender: mpsc::SyncSender<AudioFrame>,
    sample_rate: u32,
    channels: u16,
}

impl AudioPlayer {
    /// Start an audio output stream on the default device.
    ///
    /// # Errors
    ///
    /// Returns [`PlaybackError`] if no output device is available, the device
    /// config cannot be read, or the audio stream cannot be built/started.
    pub fn start() -> Result<Self, PlaybackError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(PlaybackError::NoDevice)?;

        let config = device
            .default_output_config()
            .map_err(|e| PlaybackError::Config(e.to_string()))?;

        let sample_rate = config.sample_rate().0;
        let channels = config.channels();

        // Bounded channel to avoid unbounded buffering
        let (tx, rx) = mpsc::sync_channel::<AudioFrame>(32);

        let stream = build_output_stream(&device, &config.into(), rx)?;
        stream
            .play()
            .map_err(|e| PlaybackError::Stream(e.to_string()))?;

        Ok(Self {
            _stream: stream,
            sender: tx,
            sample_rate,
            channels,
        })
    }

    /// Submit an audio frame for playback. Non-blocking; drops if buffer is full.
    ///
    /// The frame is resampled/remixed to match the output device's actual negotiated
    /// sample rate and channel count first -- devices are not guaranteed to run at the
    /// decoder's fixed 48kHz/stereo output (e.g. many audio interfaces default to
    /// 44100Hz), and writing samples at the wrong rate straight into the output buffer
    /// produces audible noise rather than a clean error.
    pub fn play(&self, frame: AudioFrame) {
        let frame = if frame.sample_rate == self.sample_rate && frame.channels == self.channels {
            frame
        } else {
            let data = remix_and_resample(
                &frame.data,
                frame.channels,
                frame.sample_rate,
                self.channels,
                self.sample_rate,
            );
            AudioFrame {
                sample_rate: self.sample_rate,
                channels: self.channels,
                data,
                timestamp_us: frame.timestamp_us,
            }
        };
        let _ = self.sender.try_send(frame);
    }

    #[must_use]
    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    #[must_use]
    pub const fn channels(&self) -> u16 {
        self.channels
    }
}

/// Remix `data` (interleaved, `from_channels`-wide, at `from_rate`) to `to_channels` at
/// `to_rate`, returning interleaved f32 samples ready to write straight into an output buffer
/// of that format.
fn remix_and_resample(
    data: &[f32],
    from_channels: u16,
    from_rate: u32,
    to_channels: u16,
    to_rate: u32,
) -> Vec<f32> {
    let remixed = remix_channels(data, from_channels, to_channels);
    if from_rate == to_rate {
        return remixed;
    }
    resample_interleaved(&remixed, to_channels, from_rate, to_rate)
}

/// Remix interleaved `data` from `from_channels` to `to_channels`.
///
/// Only mono<->stereo conversions are meaningfully handled (stereo is downmixed by averaging
/// L+R, mono is upmixed by duplicating to both channels); any other combination falls back to
/// per-frame truncation/repetition of the first channel, since decoded audio in this codebase
/// is always mono or stereo in practice.
fn remix_channels(data: &[f32], from_channels: u16, to_channels: u16) -> Vec<f32> {
    if from_channels == to_channels {
        return data.to_vec();
    }
    let from_channels = usize::from(from_channels).max(1);
    let to_channels = usize::from(to_channels).max(1);
    let frame_count = data.len().checked_div(from_channels).unwrap_or(0);
    let mut out = Vec::with_capacity(frame_count.saturating_mul(to_channels));

    for frame in data.chunks(from_channels) {
        #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
        let frame_len = frame.len().max(1) as f32;
        let mono = frame.iter().copied().sum::<f32>() / frame_len;
        let first = frame.first().copied().unwrap_or(0.0);
        for ch in 0..to_channels {
            out.push(if from_channels == 1 || to_channels == 1 {
                mono
            } else if ch < from_channels {
                frame.get(ch).copied().unwrap_or(first)
            } else {
                first
            });
        }
    }
    out
}

/// Linearly resample interleaved `data` (`channels`-wide) from `from_rate` to `to_rate`.
fn resample_interleaved(data: &[f32], channels: u16, from_rate: u32, to_rate: u32) -> Vec<f32> {
    let channels = usize::from(channels).max(1);
    let in_frames = data.len().checked_div(channels).unwrap_or(0);
    if in_frames == 0 || from_rate == 0 {
        return Vec::new();
    }
    let ratio = f64::from(to_rate) / f64::from(from_rate);
    // Truncating f64 -> usize is intentional here: out_frames/src_index are sample-buffer
    // offsets derived from a rate ratio applied to an in-memory buffer length, both always
    // far below usize/f64 precision limits for real-time audio frame sizes.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::as_conversions
    )]
    let out_frames = (in_frames as f64 * ratio).round().max(0.0) as usize;
    let mut out = Vec::with_capacity(out_frames.saturating_mul(channels));

    for out_i in 0..out_frames {
        #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
        let src_pos = out_i as f64 / ratio;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::as_conversions)]
        let src_index = src_pos.floor() as usize;
        #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
        let frac = (src_pos - src_pos.floor()) as f32;
        let next_index = src_index.saturating_add(1).min(in_frames.saturating_sub(1));

        for ch in 0..channels {
            let a = data
                .get(src_index.saturating_mul(channels).saturating_add(ch))
                .copied()
                .unwrap_or(0.0);
            let b = data
                .get(next_index.saturating_mul(channels).saturating_add(ch))
                .copied()
                .unwrap_or(a);
            out.push((b - a).mul_add(frac, a));
        }
    }
    out
}

fn build_output_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    rx: mpsc::Receiver<AudioFrame>,
) -> Result<Stream, PlaybackError> {
    let err_fn = |err| tracing::error!("Audio playback error: {err}");

    // Buffer for samples from received frames
    let mut sample_buf: Vec<f32> = Vec::new();
    let mut buf_pos = 0usize;

    let stream = device
        .build_output_stream(
            config,
            move |output: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                let mut written = 0;
                while written < output.len() {
                    // Refill buffer if needed
                    if buf_pos >= sample_buf.len() {
                        if let Ok(frame) = rx.try_recv() {
                            sample_buf = frame.data;
                            buf_pos = 0;
                        } else {
                            // No data available — output silence
                            if let Some(rest) = output.get_mut(written..) {
                                for sample in rest {
                                    *sample = 0.0;
                                }
                            }
                            return;
                        }
                    }

                    let Some(available) = sample_buf.len().checked_sub(buf_pos) else {
                        return;
                    };
                    let Some(needed) = output.len().checked_sub(written) else {
                        return;
                    };
                    let to_copy = available.min(needed);
                    let Some(buf_pos_end) = buf_pos.checked_add(to_copy) else {
                        return;
                    };
                    let Some(written_end) = written.checked_add(to_copy) else {
                        return;
                    };

                    let (Some(dest), Some(src)) = (
                        output.get_mut(written..written_end),
                        sample_buf.get(buf_pos..buf_pos_end),
                    ) else {
                        return;
                    };
                    dest.copy_from_slice(src);
                    buf_pos = buf_pos_end;
                    written = written_end;
                }
            },
            err_fn,
            None,
        )
        .map_err(|e| PlaybackError::Stream(e.to_string()))?;

    Ok(stream)
}

#[cfg(test)]
mod resample_tests {
    use super::*;

    #[test]
    fn same_rate_same_channels_is_passthrough() {
        let data = vec![0.1, 0.2, 0.3, 0.4];
        let out = remix_and_resample(&data, 2, 48000, 2, 48000);
        assert_eq!(out, data);
    }

    #[test]
    fn resample_48k_to_44_1k_shrinks_frame_count() {
        // 480 stereo frames (10ms @ 48kHz) should downsample to roughly 441 frames @ 44.1kHz.
        let data = vec![0.5f32; 480 * 2];
        let out = remix_and_resample(&data, 2, 48000, 2, 44100);
        let out_frames = out.len() / 2;
        assert!(
            (430..=450).contains(&out_frames),
            "expected ~441 output frames, got {out_frames}"
        );
    }

    #[test]
    fn resample_preserves_constant_signal_amplitude() {
        // A constant-value signal resampled should still be (approximately) that same
        // constant -- this catches gross bugs like reading garbage/misaligned samples.
        let data = vec![0.75f32; 480 * 2];
        let out = remix_and_resample(&data, 2, 48000, 2, 44100);
        assert!(out.iter().all(|&s| (s - 0.75).abs() < 1e-4));
    }

    #[test]
    fn stereo_to_mono_averages_channels() {
        let data = vec![1.0, -1.0, 0.5, 0.5];
        let out = remix_and_resample(&data, 2, 48000, 1, 48000);
        assert_eq!(out, vec![0.0, 0.5]);
    }

    #[test]
    fn mono_to_stereo_duplicates_channel() {
        let data = vec![0.3, 0.6];
        let out = remix_and_resample(&data, 1, 48000, 2, 48000);
        assert_eq!(out, vec![0.3, 0.3, 0.6, 0.6]);
    }
}
