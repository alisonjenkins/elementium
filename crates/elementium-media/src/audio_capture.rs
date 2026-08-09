use std::sync::mpsc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use elementium_types::AudioFrame;
use thiserror::Error;

/// Split from the original two variants (`Config`, `Stream`).
///
/// `Stream` conflated two distinct `cpal` calls -- building the stream and starting it -- with
/// two different error types, so a caller matching on it could not tell which had failed.
/// `Config` conflated a real `cpal` failure with "the device's sample format is one we do not
/// decode", which has no `cpal` error behind it at all -- it is our own format table being
/// incomplete, not the device misbehaving.
#[derive(Error, Debug)]
pub enum CaptureError {
    #[error("No input device available")]
    NoDevice,
    #[error("Failed to get device config: {0}")]
    Config(#[source] cpal::DefaultStreamConfigError),
    /// The device's sample format has no decode path in [`build_stream`]. Not a `cpal`
    /// failure -- `cpal` reported the format successfully; this crate just does not handle
    /// it (only `F32`/`I16`/`U16` are, `F64` and others are not).
    #[error("Unsupported audio sample format: {0:?}")]
    UnsupportedSampleFormat(SampleFormat),
    #[error("Failed to build audio stream: {0}")]
    BuildStream(#[source] cpal::BuildStreamError),
    #[error("Failed to start audio stream: {0}")]
    Play(#[source] cpal::PlayStreamError),
}

/// Captures audio from the default input device.
pub struct AudioCapturer {
    _stream: Stream,
    receiver: mpsc::Receiver<AudioFrame>,
    sample_rate: u32,
    channels: u16,
    device_id: String,
}

impl Drop for AudioCapturer {
    /// cpal logs nothing when a `Stream` is dropped -- it just stops calling back, silently.
    /// That silence is what let a pipeline restart open a second stream on the same input
    /// while the first was still alive: nothing said the first had ended, or when. Logged
    /// unconditionally rather than throttled: this fires once per capturer, not once per
    /// audio callback.
    fn drop(&mut self) {
        tracing::info!(
            device_id = %self.device_id,
            sample_rate = self.sample_rate,
            channels = self.channels,
            "audio capturer dropped; input stream released"
        );
    }
}

/// Choose the capture format for `device`, preferring Opus's own sample rate.
///
/// Prefer 48kHz explicitly rather than trusting `default_input_config()`, for the same
/// reason the playback side does -- see [`crate::stream_config`]. cpal's synthesized default
/// biases toward 44100Hz, and on this machine that is exactly what happened: capture ran at
/// 44100 while Opus encodes at 48000, so every captured chunk went through a naive linear
/// resample on the way to the encoder. Capturing at Opus's native rate removes that
/// conversion entirely.
fn input_config(
    device: &cpal::Device,
    device_id: &str,
) -> Result<cpal::SupportedStreamConfig, CaptureError> {
    device
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
            CaptureError::Config(e)
        })
}

impl AudioCapturer {
    /// Start capturing audio from the default input device.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError`] if no input device is available, the device
    /// config cannot be read, or the audio stream cannot be built/started.
    pub fn start() -> Result<Self, CaptureError> {
        Self::start_on_device(None)
    }

    /// Start capturing from a chosen input device, or the default when none is chosen.
    ///
    /// `index` is the position in `cpal`'s own `input_devices()` iteration, which is what
    /// [`crate::device_enumeration::enumerate_audio_devices`] numbers its ids by -- so the
    /// device the user picked and the device opened here come from one list rather than
    /// two. Before this, the chosen id was carried all the way down and then ignored: the
    /// default microphone was always opened, whatever the settings said.
    ///
    /// A chosen device that has since gone away falls back to the default rather than
    /// failing, because a call with the wrong microphone beats a call with none -- but the
    /// substitution is logged, since doing it silently is what made the original bug
    /// invisible.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError`] if no input device is available, the device config cannot
    /// be read, or the audio stream cannot be built or started.
    pub fn start_on_device(index: Option<usize>) -> Result<Self, CaptureError> {
        let host = cpal::default_host();
        let chosen = index.and_then(|i| {
            let found = host.input_devices().ok().and_then(|mut d| d.nth(i));
            if found.is_none() {
                tracing::warn!(
                    requested_index = i,
                    "the requested microphone is no longer present; using the default"
                );
            }
            found
        });
        let device = match chosen {
            Some(device) => device,
            None => host.default_input_device().ok_or_else(|| {
                tracing::error!(error_kind = "no_device", "No audio input device available");
                CaptureError::NoDevice
            })?,
        };

        let device_id = device.name().unwrap_or_else(|_| "unknown".to_string());

        let config = input_config(&device, &device_id)?;

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
                return Err(CaptureError::UnsupportedSampleFormat(sample_format));
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
            CaptureError::Play(e)
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
            device_id,
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
    let device_id_for_zero_check = device_id.clone();
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

    // Reported once rather than per callback: the condition is permanent and this runs in a
    // real-time audio thread.
    let consumer_gone = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // A device that stops feeding this stream -- the two-streams-on-one-input fault this
    // module now guards against on the handover path -- produces no error and no log of its
    // own; cpal keeps calling back, just with nothing in it. These three track how long that
    // has been going on so it can be reported once it means something, and reset (and
    // re-armed) the moment real samples return.
    let zero_streak_since = std::sync::Mutex::new(None::<std::time::Instant>);
    let zero_streak_buffers = std::sync::atomic::AtomicU64::new(0);
    let zero_streak_reported = std::sync::atomic::AtomicBool::new(false);
    let stream = device
        .build_input_stream(
            config,
            move |data: &[T], _info: &cpal::InputCallbackInfo| {
                let samples: Vec<f32> = data.iter().map(|&s| s.into()).collect();
                note_zero_streak(
                    &samples,
                    &device_id_for_zero_check,
                    &zero_streak_since,
                    &zero_streak_buffers,
                    &zero_streak_reported,
                );
                // A failed send means the consumer is gone while cpal keeps calling this
                // every few milliseconds: the microphone is live and nothing is listening.
                // Reported once — this is a real-time audio callback, and logging per frame
                // from inside one is its own fault.
                if tx
                    .send(AudioFrame {
                        sample_rate,
                        channels,
                        data: samples,
                        timestamp_us: 0,
                    })
                    .is_err()
                    && !consumer_gone.swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    tracing::warn!(
                        "the microphone's consumer is gone; captured audio is being discarded"
                    );
                }
            },
            err_fn,
            None,
        )
        .map_err(CaptureError::BuildStream)?;

    Ok(stream)
}

/// How long an input stream must deliver nothing but zeros before it is worth a log line.
///
/// Not the same question `MonoFold`'s decay answers -- that reports a fold's memory of a
/// peak that has already gone stale. This looks at the raw samples cpal handed the
/// callback, so it catches a stream that has stopped being fed at all (two capturers
/// contending for one input is exactly this: the loser gets called back on schedule with
/// nothing in the buffer) rather than a quiet room.
const ZERO_STREAK_WARN_AFTER: std::time::Duration = std::time::Duration::from_secs(1);

/// Below this magnitude a sample counts as exact silence rather than noise. Far tighter
/// than [`LIVE_CHANNEL_PEAK`]: this only needs to catch the literal zeros a starved
/// callback delivers, not decide whether someone is speaking.
const ZERO_SAMPLE_EPSILON: f32 = 1e-9;

/// Update the running zero-sample streak for one capture callback, logging once when it has
/// lasted long enough to mean the device stopped, not just paused between words.
///
/// Resets and re-arms on the first non-zero buffer, so a second stall later in the same
/// call is still reported rather than being silenced forever by the first one.
fn note_zero_streak(
    samples: &[f32],
    device_id: &str,
    since: &std::sync::Mutex<Option<std::time::Instant>>,
    buffers: &std::sync::atomic::AtomicU64,
    reported: &std::sync::atomic::AtomicBool,
) {
    let all_zero = samples.iter().all(|s| s.abs() <= ZERO_SAMPLE_EPSILON);
    if !all_zero {
        if let Ok(mut guard) = since.lock() {
            *guard = None;
        }
        buffers.store(0, std::sync::atomic::Ordering::Relaxed);
        reported.store(false, std::sync::atomic::Ordering::Relaxed);
        return;
    }

    let Ok(mut guard) = since.lock() else {
        return;
    };
    let started = *guard.get_or_insert_with(std::time::Instant::now);
    let streak = buffers.fetch_add(1, std::sync::atomic::Ordering::Relaxed).saturating_add(1);
    let elapsed = started.elapsed();
    if elapsed >= ZERO_STREAK_WARN_AFTER
        && !reported.swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        tracing::warn!(
            device_id = %device_id,
            zero_buffers = streak,
            streak_ms = elapsed.as_millis(),
            "audio input stream has delivered only zero samples for a sustained period; \
             the device may have stopped feeding this capture (e.g. another stream still \
             holds it)"
        );
    }
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
/// A channel is carrying signal if its recent peak has been above this.
///
/// About -66 dBFS: below any microphone's noise floor in a room with someone in it, and
/// far above the exact zeros an unconnected input produces.
const LIVE_CHANNEL_PEAK: f32 = 0.0005;

/// How much of the remembered peak survives each buffer.
///
/// At one 20ms buffer per callback this forgets a channel over roughly two seconds, so a
/// pause between words does not make a channel look dead, and unplugging a microphone is
/// noticed before the call ends.
const PEAK_DECAY: f32 = 0.99;

/// Folds a multi-channel microphone down to the one channel everything downstream expects,
/// averaging only the channels that are actually carrying something.
///
/// Averaging every channel is right for a stereo microphone and wrong for an audio
/// interface. A Scarlett 2i2 with one microphone in input 1 presents two channels, the
/// second of which is silence, and averaging them attenuates the speaker by exactly 6 dB.
/// That is the difference between Opus encoding speech and Opus deciding the input is
/// near-silence and emitting three-byte frames -- which is what "they cannot hear me, I
/// break up" sounded like from the far end, with nothing dropped and nothing late.
///
/// Deliberately not applied to shared desktop audio, which is genuinely stereo: there,
/// averaging is the correct fold and a channel that falls quiet is music, not a missing
/// cable.
pub struct MonoFold {
    channels: u16,
    /// Decaying peak per channel, in the order the device interleaves them.
    peaks: Vec<f32>,
}

impl MonoFold {
    #[must_use]
    pub fn new(channels: u16) -> Self {
        Self {
            channels,
            peaks: vec![0.0; usize::from(channels.max(1))],
        }
    }

    /// The remembered peak of each channel, for reporting which input the voice is on.
    #[must_use]
    pub fn channel_peaks(&self) -> &[f32] {
        &self.peaks
    }

    /// Fold one interleaved buffer to mono.
    pub fn fold(&mut self, interleaved: &[f32]) -> Vec<f32> {
        let ch = usize::from(self.channels);
        if ch <= 1 {
            return interleaved.to_vec();
        }
        for peak in &mut self.peaks {
            *peak *= PEAK_DECAY;
        }
        for frame in interleaved.chunks_exact(ch) {
            for (index, sample) in frame.iter().enumerate() {
                if let Some(peak) = self.peaks.get_mut(index) {
                    *peak = peak.max(sample.abs());
                }
            }
        }

        let live: Vec<bool> = self.peaks.iter().map(|p| *p > LIVE_CHANNEL_PEAK).collect();
        let live_count = live.iter().filter(|l| **l).count();
        // Nothing anywhere: the buffer is silence whichever way it is folded, and treating
        // every channel as live keeps the arithmetic identical to the plain average.
        let (divisor, use_all) = if live_count == 0 {
            (ch, true)
        } else {
            (live_count, false)
        };
        // Channel counts are single digits, so the widening is exact.
        let divisor = u16::try_from(divisor.max(1)).map_or(1.0_f32, f32::from);

        interleaved
            .chunks_exact(ch)
            .map(|frame| {
                let sum: f32 = frame
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| use_all || live.get(*i).copied().unwrap_or(true))
                    .map(|(_, s)| *s)
                    .sum();
                sum / divisor
            })
            .collect()
    }
}

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
#[allow(clippy::indexing_slicing)]
mod mono_fold_tests {
    use super::{MonoFold, downmix_to_mono};

    /// One 20ms buffer at 48kHz, with `left` on channel 0 and `right` on channel 1.
    fn stereo(left: f32, right: f32) -> Vec<f32> {
        (0..960).flat_map(|_| [left, right]).collect()
    }

    /// The bug this exists for: an audio interface with one microphone in it.
    ///
    /// Two channels, one of them an unconnected input, and the plain average hands the
    /// encoder exactly half the speaker's amplitude -- 6 dB, which is the difference
    /// between Opus encoding speech and Opus calling it silence.
    #[test]
    fn a_silent_second_input_does_not_halve_the_microphone() {
        let mut fold = MonoFold::new(2);
        // Long enough for the level history to settle, as it would within a word.
        let mut out = Vec::new();
        for _ in 0..10 {
            out = fold.fold(&stereo(0.5, 0.0));
        }
        assert!(
            (out[0] - 0.5).abs() < 1e-6,
            "the microphone should arrive at its own level, got {}",
            out[0]
        );
        assert!(
            (downmix_to_mono(&stereo(0.5, 0.0), 2)[0] - 0.25).abs() < 1e-6,
            "the plain average is what halved it, and is kept for genuinely stereo sources"
        );
    }

    /// A real stereo microphone must still be averaged, or a loud left side alone would
    /// come through at full scale and clip against a loud right side later.
    #[test]
    fn a_genuinely_stereo_source_is_still_averaged() {
        let mut fold = MonoFold::new(2);
        let mut out = Vec::new();
        for _ in 0..10 {
            out = fold.fold(&stereo(0.6, 0.2));
        }
        assert!(
            (out[0] - 0.4).abs() < 1e-6,
            "both channels carry signal, so both count: got {}",
            out[0]
        );
    }

    /// Silence must fold to silence whichever rule applies, with no divide by zero.
    #[test]
    fn silence_stays_silence() {
        let mut fold = MonoFold::new(2);
        let out = fold.fold(&stereo(0.0, 0.0));
        assert!(out.iter().all(|s| *s == 0.0));
    }

    /// A channel that falls quiet mid-call is remembered for a couple of seconds, so a
    /// pause between words does not change the gain of the next one.
    #[test]
    fn a_pause_does_not_immediately_drop_a_channel() {
        let mut fold = MonoFold::new(2);
        for _ in 0..10 {
            let _ = fold.fold(&stereo(0.6, 0.4));
        }
        let out = fold.fold(&stereo(0.6, 0.0));
        assert!(
            (out[0] - 0.3).abs() < 1e-6,
            "channel 1 was live a moment ago and still counts: got {}",
            out[0]
        );
    }

    /// Mono input is passed straight through: there is nothing to decide.
    #[test]
    fn a_mono_device_is_untouched() {
        let mut fold = MonoFold::new(1);
        let out = fold.fold(&[0.1, 0.2, 0.3]);
        assert_eq!(out, vec![0.1, 0.2, 0.3]);
    }
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

#[cfg(test)]
mod capture_error_tests {
    use super::CaptureError;

    /// Pins the split of the old single `Config(String)`: a `cpal` config failure must carry
    /// the real `DefaultStreamConfigError` as its source, not just a rendered message.
    #[test]
    fn config_failure_carries_the_cpal_error_as_its_source() {
        let err = CaptureError::Config(cpal::DefaultStreamConfigError::DeviceNotAvailable);
        assert!(matches!(err, CaptureError::Config(_)));
        assert!(std::error::Error::source(&err).is_some());
    }

    /// A sample format `cpal` reports successfully but this crate has no decode path for is
    /// not a `cpal` failure at all -- it must be its own variant with no source, distinct from
    /// `Config`, which was the old (wrong) home for this string.
    #[test]
    fn unsupported_sample_format_has_no_underlying_source() {
        let err = CaptureError::UnsupportedSampleFormat(cpal::SampleFormat::F64);
        assert!(matches!(err, CaptureError::UnsupportedSampleFormat(_)));
        assert!(std::error::Error::source(&err).is_none());
    }

    /// Pins the split of the old single `Stream(String)`: building the stream and starting it
    /// are two different `cpal` calls with two different error types, and must be
    /// distinguishable -- `BuildStream` for the former.
    #[test]
    fn build_stream_failure_carries_the_cpal_error_as_its_source() {
        let err = CaptureError::BuildStream(cpal::BuildStreamError::DeviceNotAvailable);
        assert!(matches!(err, CaptureError::BuildStream(_)));
        assert!(std::error::Error::source(&err).is_some());
    }

    /// The other half of the same split: starting an already-built stream (`Stream::play`).
    #[test]
    fn play_failure_carries_the_cpal_error_as_its_source() {
        let err = CaptureError::Play(cpal::PlayStreamError::DeviceNotAvailable);
        assert!(matches!(err, CaptureError::Play(_)));
        assert!(std::error::Error::source(&err).is_some());
    }
}
