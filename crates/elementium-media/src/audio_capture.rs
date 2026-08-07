use std::sync::mpsc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use elementium_types::AudioFrame;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum CaptureError {
    #[error("No input device available")]
    NoDevice,
    #[error("Failed to get device config: {0}")]
    Config(String),
    #[error("Failed to build stream: {0}")]
    Stream(String),
}

/// Captures audio from the default input device.
pub struct AudioCapturer {
    _stream: Stream,
    receiver: mpsc::Receiver<AudioFrame>,
    sample_rate: u32,
    channels: u16,
}

impl AudioCapturer {
    /// Start capturing audio from the default input device.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError`] if no input device is available, the device
    /// config cannot be read, or the audio stream cannot be built/started.
    pub fn start() -> Result<Self, CaptureError> {
        let host = cpal::default_host();
        let device = host.default_input_device().ok_or_else(|| {
            tracing::error!(error_kind = "no_device", "No audio input device available");
            CaptureError::NoDevice
        })?;

        let device_id = device.name().unwrap_or_else(|_| "unknown".to_string());

        // Prefer 48kHz explicitly rather than trusting `default_input_config()`, for the
        // same reason the playback side does -- see `crate::stream_config`. cpal's
        // synthesized default biases toward 44100Hz, and on this machine that is exactly
        // what happened: capture ran at 44100 while Opus encodes at 48000, so every single
        // captured chunk went through a naive linear resample on the way to the encoder.
        // Capturing at Opus's native rate removes that conversion entirely.
        let config = device
            .supported_input_configs()
            .ok()
            .and_then(|configs| {
                crate::stream_config::pick_preferred_config(
                    configs,
                    crate::stream_config::PREFERRED_RATE,
                )
            })
            .map_or_else(
                || {
                    tracing::warn!(
                        device_id = %device_id,
                        preferred_rate = crate::stream_config::PREFERRED_RATE,
                        "Input device does not advertise the preferred capture rate; \
                         falling back to its default (captured audio will be resampled)"
                    );
                    device.default_input_config()
                },
                Ok,
            )
            .map_err(|e| {
                tracing::error!(
                    device_id = %device_id,
                    error_kind = "config",
                    error = %e,
                    "Failed to get audio input device config"
                );
                CaptureError::Config(e.to_string())
            })?;

        let sample_rate = config.sample_rate().0;
        let channels = config.channels();
        let sample_format = config.sample_format();
        let (tx, rx) = mpsc::channel();

        // Ask the device for exactly one Opus frame per callback, rather than accepting
        // whatever it defaults to. See `opus_frame_buffer_size` for why this is not
        // cosmetic.
        let buffer_size = opus_frame_buffer_size(config.buffer_size(), sample_rate);
        let mut stream_config: cpal::StreamConfig = config.clone().into();
        stream_config.buffer_size = buffer_size;
        tracing::info!(
            device_id = %device_id,
            sample_rate,
            channels,
            requested_buffer = ?buffer_size,
            supported_buffer = ?config.buffer_size(),
            "Audio input buffer size selected"
        );

        let stream = match sample_format {
            SampleFormat::F32 => {
                build_stream::<f32>(&device, &stream_config, tx, sample_rate, channels)
            }
            SampleFormat::I16 => {
                build_stream::<i16>(&device, &stream_config, tx, sample_rate, channels)
            }
            SampleFormat::U16 => {
                build_stream::<u16>(&device, &stream_config, tx, sample_rate, channels)
            }
            _ => {
                tracing::error!(
                    device_id = %device_id,
                    sample_rate,
                    channels,
                    sample_format = ?sample_format,
                    error_kind = "unsupported_sample_format",
                    "Unsupported audio sample format"
                );
                return Err(CaptureError::Config("unsupported sample format".into()));
            }
        }
        .map_err(|e| {
            tracing::error!(
                device_id = %device_id,
                sample_rate,
                channels,
                error_kind = "build_stream",
                error = %e,
                "Failed to build audio input stream"
            );
            e
        })?;

        stream.play().map_err(|e| {
            tracing::error!(
                device_id = %device_id,
                error_kind = "stream_play",
                error = %e,
                "Failed to start audio input stream"
            );
            CaptureError::Stream(e.to_string())
        })?;

        tracing::info!(
            device_id = %device_id,
            sample_rate,
            channels,
            "Audio input device started"
        );

        Ok(Self {
            _stream: stream,
            receiver: rx,
            sample_rate,
            channels,
        })
    }

    /// Receive the next audio frame (blocking).
    #[must_use]
    pub fn recv(&self) -> Option<AudioFrame> {
        self.receiver.recv().ok()
    }

    /// Try to receive an audio frame (non-blocking).
    #[must_use]
    pub fn try_recv(&self) -> Option<AudioFrame> {
        self.receiver.try_recv().ok()
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

/// Choose an input buffer size of exactly one 20ms Opus frame, if the device allows it.
///
/// Not cosmetic: with `BufferSize::Default` the device picks, and on a real machine it
/// picked ~30ms. Because a 30ms buffer is not a multiple of Opus's 20ms frame, the
/// capture loop emits frames unevenly -- measured as roughly one frame in four arriving
/// less than 5ms after the previous one, with gaps of 30ms between clumps.
///
/// That matters because str0m runs with `PacerImpl::null()` (we do not enable BWE), so it
/// transmits each packet the instant it is handed over. Clumped production therefore
/// becomes clumped transmission: the far end receives bursts instead of a steady 50
/// packets/sec, and a jitter buffer sized for a smooth stream underruns between clumps
/// and fills the gaps with packet-loss concealment. The result sounds robotic while every
/// diagnostic stays clean -- RTCP reports 0% loss, because nothing is actually lost.
///
/// Falls back to the device default when the device cannot honour the request; a device
/// that dictates its own buffer size is better than a failed stream.
fn opus_frame_buffer_size(
    supported: &cpal::SupportedBufferSize,
    sample_rate: u32,
) -> cpal::BufferSize {
    // 20ms, the frame size everything downstream (encoder, RTP timestamps) assumes.
    let desired = sample_rate / 50;
    match supported {
        cpal::SupportedBufferSize::Range { min, max } if (*min..=*max).contains(&desired) => {
            cpal::BufferSize::Fixed(desired)
        }
        _ => cpal::BufferSize::Default,
    }
}

fn build_stream<T: cpal::Sample + cpal::SizedSample + Into<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    tx: mpsc::Sender<AudioFrame>,
    sample_rate: u32,
    channels: u16,
) -> Result<Stream, CaptureError> {
    let device_id = device.name().unwrap_or_else(|_| "unknown".to_string());
    let err_fn = move |err| {
        tracing::error!(
            device_id = %device_id,
            sample_rate,
            channels,
            error_kind = "stream_callback",
            error = %err,
            "Audio capture stream error"
        );
    };

    let stream = device
        .build_input_stream(
            config,
            move |data: &[T], _info: &cpal::InputCallbackInfo| {
                let samples: Vec<f32> = data.iter().map(|&s| s.into()).collect();
                let _ = tx.send(AudioFrame {
                    sample_rate,
                    channels,
                    data: samples,
                    timestamp_us: 0,
                });
            },
            err_fn,
            None,
        )
        .map_err(|e| CaptureError::Stream(e.to_string()))?;

    Ok(stream)
}

/// Fold an interleaved capture buffer down to a single channel by averaging.
///
/// Voice is sent as mono. Opus splits a fixed bitrate across the channels it is given, so
/// encoding a microphone as stereo halves the bits available to the only content that
/// matters and signals nothing useful -- a microphone's two channels are near-identical.
/// Mono is also what the SDP already claims: RFC 7587 defaults `sprop-stereo` to 0, and
/// nothing in the offer/answer path ever set it, so a stereo stream was being described to
/// every receiver as mono.
///
/// Averaging rather than dropping a channel keeps whatever is only present on one side of
/// a stereo capture, and cannot clip: the mean of values in `-1.0..=1.0` stays in range.
///
/// `channels` of 0 or 1 returns the input unchanged.
#[must_use]
pub fn downmix_to_mono(interleaved: &[f32], channels: u16) -> Vec<f32> {
    let ch = usize::from(channels);
    if ch <= 1 {
        return interleaved.to_vec();
    }
    // Channel counts are tiny (1..=32 in practice) and f32 represents every integer up to
    // 2^24 exactly, so this is lossless; `f32::from(u16)` would be exact but `ch` is a
    // usize by this point and `u16::try_from` cannot fail for a value we just widened.
    let divisor = f32::from(channels.max(1));
    interleaved
        .chunks_exact(ch)
        .map(|frame| frame.iter().sum::<f32>() / divisor)
        .collect()
}

#[cfg(test)]
mod buffer_size_tests {
    use super::*;

    /// The real bug: accepting the device default gave a ~30ms buffer, which is not a
    /// multiple of Opus's 20ms frame, so frames were produced in clumps and transmitted
    /// as clumps. One frame per callback is what makes the output a steady stream.
    #[test]
    fn requests_exactly_one_opus_frame_when_the_device_allows_it() {
        let supported = cpal::SupportedBufferSize::Range { min: 64, max: 4096 };
        assert_eq!(
            opus_frame_buffer_size(&supported, 48_000),
            cpal::BufferSize::Fixed(960),
            "48kHz: 20ms is 960 frames"
        );
        assert_eq!(
            opus_frame_buffer_size(&supported, 24_000),
            cpal::BufferSize::Fixed(480),
            "the frame size must follow the sample rate, not be hardcoded"
        );
    }

    /// A device that cannot go as small as one frame must not get an out-of-range request
    /// -- a working stream on the device's own terms beats a failed one.
    #[test]
    fn falls_back_to_the_device_default_when_one_frame_is_out_of_range() {
        let too_coarse = cpal::SupportedBufferSize::Range {
            min: 2048,
            max: 4096,
        };
        assert_eq!(
            opus_frame_buffer_size(&too_coarse, 48_000),
            cpal::BufferSize::Default
        );

        let too_fine = cpal::SupportedBufferSize::Range { min: 16, max: 128 };
        assert_eq!(
            opus_frame_buffer_size(&too_fine, 48_000),
            cpal::BufferSize::Default
        );
    }

    /// Some backends do not report a range at all.
    #[test]
    fn falls_back_when_the_device_reports_no_range() {
        assert_eq!(
            opus_frame_buffer_size(&cpal::SupportedBufferSize::Unknown, 48_000),
            cpal::BufferSize::Default
        );
    }

    /// Boundary values must be usable, not rejected by an off-by-one.
    #[test]
    fn accepts_a_frame_size_exactly_at_the_range_boundary() {
        assert_eq!(
            opus_frame_buffer_size(
                &cpal::SupportedBufferSize::Range {
                    min: 960,
                    max: 4096
                },
                48_000
            ),
            cpal::BufferSize::Fixed(960)
        );
        assert_eq!(
            opus_frame_buffer_size(
                &cpal::SupportedBufferSize::Range { min: 64, max: 960 },
                48_000
            ),
            cpal::BufferSize::Fixed(960)
        );
    }
}

#[cfg(test)]
mod downmix_tests {
    use super::downmix_to_mono;

    #[test]
    fn averages_the_channels_of_each_frame() {
        // Two frames of stereo: (1.0, 0.0) and (0.5, -0.5).
        let out = downmix_to_mono(&[1.0, 0.0, 0.5, -0.5], 2);
        assert_eq!(out, vec![0.5, 0.0]);
    }

    #[test]
    fn leaves_mono_input_untouched() {
        let mono = [0.25f32, -0.75, 1.0];
        assert_eq!(downmix_to_mono(&mono, 1), mono.to_vec());
        assert_eq!(downmix_to_mono(&mono, 0), mono.to_vec());
    }

    #[test]
    fn handles_more_than_two_channels() {
        let out = downmix_to_mono(&[1.0, 1.0, 1.0, 1.0], 4);
        assert_eq!(out, vec![1.0]);
    }

    #[test]
    fn drops_a_trailing_partial_frame_rather_than_emitting_a_wrong_sample() {
        // A partial frame averaged over the full channel count would be attenuated, which
        // is an audible click rather than an honest omission.
        let out = downmix_to_mono(&[1.0, 1.0, 1.0], 2);
        assert_eq!(out, vec![1.0]);
    }

    #[test]
    fn output_length_is_the_input_divided_by_the_channel_count() {
        let input = vec![0.1f32; 960 * 2];
        assert_eq!(downmix_to_mono(&input, 2).len(), 960);
    }

    #[test]
    fn a_full_scale_signal_cannot_clip() {
        let out = downmix_to_mono(&[1.0, 1.0, -1.0, -1.0], 2);
        assert!(out.iter().all(|s| (-1.0..=1.0).contains(s)), "{out:?}");
    }
}
