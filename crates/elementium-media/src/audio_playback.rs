use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

use cpal::Stream;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
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

/// Process-wide count of currently-open `AudioPlayer` output streams.
///
/// Observability for a real suspected bug: if more than one `AudioPlayer` is ever open
/// at once, two independent cpal output streams are both writing to (or contending for)
/// the same physical output device concurrently, which can itself glitch/distort audio
/// independently of whether either stream's decoded content is correct. Logged on every
/// open/close so this is visible in the structured logs instead of having to be inferred.
static ACTIVE_PLAYER_COUNT: AtomicUsize = AtomicUsize::new(0);
static NEXT_PLAYER_ID: AtomicU64 = AtomicU64::new(1);

/// The number of `AudioPlayer` output streams currently open, process-wide.
///
/// Exposed so callers (and tests) can assert on it directly instead of only inferring it
/// from logs -- see the real bug this pins: every `PeerConnection` used to unconditionally
/// open its own `AudioPlayer`, so a single call with two `PeerConnection`s (e.g. two
/// participants' tracks bundled across connections in a mesh call) opened two independent
/// output streams to the same physical device concurrently.
#[must_use]
pub fn active_player_count() -> usize {
    ACTIVE_PLAYER_COUNT.load(Ordering::Relaxed)
}

/// Plays audio to the default output device.
///
/// Holds a `cpal::Stream`, which is deliberately `!Send`/`!Sync` (see
/// `cpal::platform::NotSendSyncAcrossAllPlatforms`) -- it cannot be moved to or shared
/// across threads. To let many threads submit audio to one physical output stream (see
/// [`start_shared`]), get a [`AudioSink`] via [`AudioPlayer::sink`] instead: it holds only
/// a plain, `Send + Sync`, cloneable channel sender, no `Stream`.
pub struct AudioPlayer {
    _stream: Stream,
    sender: mpsc::SyncSender<(String, AudioFrame)>,
    sample_rate: u32,
    channels: u16,
    id: u64,
    /// Count of frames dropped because the playback thread's buffer was full -- a real,
    /// previously-invisible failure mode: if decode/playback falls behind, frames are
    /// silently discarded rather than queued, which can be heard as stutter/glitches.
    /// Logged (throttled) so a real backlog is visible in logs. `Arc`-shared with every
    /// [`AudioSink`] cloned from this player so all feeders contribute to one true count.
    dropped_frames: Arc<AtomicU64>,
}

/// A cheap, cloneable handle for submitting audio frames to a shared [`AudioPlayer`]'s
/// output stream from any thread.
///
/// Exists because `AudioPlayer` itself can't be shared across threads (see its doc
/// comment) -- this holds only the plain channel sender and playback metadata needed to
/// resample/submit a frame, which are all `Send + Sync`.
#[derive(Clone)]
pub struct AudioSink {
    sender: mpsc::SyncSender<(String, AudioFrame)>,
    sample_rate: u32,
    channels: u16,
    player_id: u64,
    dropped_frames: Arc<AtomicU64>,
}

impl AudioSink {
    /// Submit an audio frame for playback. Non-blocking; drops if the buffer is full.
    ///
    /// `track_id` must uniquely identify the remote track this frame belongs to (e.g.
    /// `"{pc_id}-{mid}"`). It is **required, not optional**: the mixer plays a single
    /// track's frames sequentially but sums different tracks together, and it can only
    /// tell those cases apart by this id. Passing a shared/constant id for genuinely
    /// distinct tracks time-division-multiplexes them; passing a *varying* id for one
    /// track's consecutive frames superimposes that track on itself at multiplied speed.
    /// Both are audible as "digital screeching" -- see `mix_tracks_into`.
    ///
    /// The frame is resampled/remixed to match the output device's actual negotiated
    /// sample rate and channel count first -- devices are not guaranteed to run at the
    /// decoder's fixed 48kHz/stereo output (e.g. many audio interfaces default to
    /// 44100Hz), and writing samples at the wrong rate straight into the output buffer
    /// produces audible noise rather than a clean error.
    pub fn play(&self, track_id: &str, frame: AudioFrame) {
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
        if self.sender.try_send((track_id.to_string(), frame)).is_err() {
            let dropped = self
                .dropped_frames
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            // Throttled: this can legitimately fire often under sustained backlog, and
            // logging every single drop would itself perturb timing.
            if dropped.is_multiple_of(50) {
                tracing::warn!(
                    player_id = self.player_id,
                    track_id,
                    dropped_frames = dropped,
                    "AudioPlayer dropped a frame: playback buffer full"
                );
            }
        }
    }

    /// Whether this handle and `other` refer to the same underlying output stream.
    ///
    /// Compares the shared `dropped_frames` counter's pointer identity: two [`AudioSink`]
    /// clones from the same [`AudioPlayer`] always share that one `Arc`, so pointer
    /// equality on it is exactly "same stream" -- used to test the [`shared_sink`]
    /// singleton invariant without exposing the private `player_id`.
    #[must_use]
    pub fn same_stream_as(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.dropped_frames, &other.dropped_frames)
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

/// Get the single, process-wide shared audio output stream, starting it on first call.
///
/// A genuine process-wide singleton (not per-caller-site caching): every call site
/// anywhere in the process -- the native WebRTC engine, the `LiveKit` subscriber path,
/// any future caller -- gets a clone of the exact same [`AudioSink`], so it is
/// structurally impossible for two independent output streams to exist at once
/// regardless of which subsystems happen to be active simultaneously. That guarantee is
/// the actual fix for a real confirmed bug: two `PeerConnection`s in one call each used
/// to open their own output stream, and two independent cpal streams writing to the same
/// physical device concurrently caused audio glitching independent of decode
/// correctness.
///
/// Returns `None` (logged once) if no output device was available on the first call --
/// cached, not retried, matching the pre-existing per-call-site behavior.
///
/// The underlying [`AudioPlayer`] (and its `!Send` `cpal::Stream`) is intentionally
/// leaked on first success: once `stream.play()` has been called, nothing needs to touch
/// the `AudioPlayer` again (cpal drives playback from its own internal callback thread
/// from then on) -- this is meant to be a process-lifetime singleton, so leaking it is
/// the correct lifecycle, not a bug.
pub fn shared_sink() -> Option<AudioSink> {
    static SHARED: std::sync::OnceLock<Option<AudioSink>> = std::sync::OnceLock::new();
    SHARED
        .get_or_init(|| match AudioPlayer::start() {
            Ok(player) => {
                let sink = player.sink();
                // Deliberate: see doc comment above.
                Box::leak(Box::new(player));
                Some(sink)
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to start shared audio playback stream");
                None
            }
        })
        .clone()
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

        // `default_output_config()` is not a query of the device's actual running rate --
        // cpal's own `cmp_default_heuristics` (used to synthesize a "default" when the
        // backend doesn't report one) explicitly biases toward 44100Hz ("CD quality") over
        // any other supported rate. On Linux, "default" almost always routes through
        // PipeWire's ALSA compatibility shim, which runs its whole graph at one fixed rate
        // (commonly 48000Hz) regardless of what an individual ALSA client asks for --
        // ALSA's `set_rate(..., ValueOr::Nearest)` lets it silently substitute a different
        // rate than requested, and cpal never reads the negotiated rate back afterward. If
        // the graph is actually running at 48000Hz while every layer above (our own
        // resampler, cpal's `StreamConfig`) believes it's 44100Hz, that's a ~9% sample-rate
        // mismatch -- audibly indistinguishable from harsh digital noise. Explicitly
        // preferring 48000Hz (Opus's native output rate, so this also removes our own
        // resample step in the common case) when the device advertises it as supported
        // sidesteps cpal's 44100 bias instead of trusting it blindly.
        let config = device
            .supported_output_configs()
            .ok()
            .and_then(|configs| {
                crate::stream_config::pick_preferred_config(
                    configs,
                    crate::stream_config::PREFERRED_RATE,
                )
            })
            .map_or_else(|| device.default_output_config(), Ok)
            .map_err(|e| PlaybackError::Config(e.to_string()))?;

        let sample_rate = config.sample_rate().0;
        let channels = config.channels();
        let device_name = device.name().unwrap_or_else(|_| "unknown".to_string());

        // Bounded channel to avoid unbounded buffering
        let (tx, rx) = mpsc::sync_channel::<(String, AudioFrame)>(32);

        let stream = build_output_stream(&device, &config.into(), rx)?;
        stream
            .play()
            .map_err(|e| PlaybackError::Stream(e.to_string()))?;

        let id = NEXT_PLAYER_ID.fetch_add(1, Ordering::Relaxed);
        let active_now = ACTIVE_PLAYER_COUNT
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        tracing::info!(
            player_id = id,
            device = %device_name,
            sample_rate,
            channels,
            active_player_count = active_now,
            "AudioPlayer output stream opened"
        );
        if active_now > 1 {
            tracing::warn!(
                player_id = id,
                active_player_count = active_now,
                "More than one AudioPlayer output stream open concurrently -- \
                 independent streams writing to the same physical output device can \
                 glitch/distort audio regardless of whether each stream's decoded \
                 content is correct"
            );
        }

        Ok(Self {
            _stream: stream,
            sender: tx,
            sample_rate,
            channels,
            id,
            dropped_frames: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Get a cheap, cloneable, `Send + Sync` handle for submitting frames to this
    /// player's output stream from any thread. See [`AudioSink`].
    #[must_use]
    pub fn sink(&self) -> AudioSink {
        AudioSink {
            sender: self.sender.clone(),
            sample_rate: self.sample_rate,
            channels: self.channels,
            player_id: self.id,
            dropped_frames: self.dropped_frames.clone(),
        }
    }

    /// Submit an audio frame for playback. Non-blocking; drops if buffer is full.
    ///
    /// The frame is resampled/remixed to match the output device's actual negotiated
    /// sample rate and channel count first -- devices are not guaranteed to run at the
    /// decoder's fixed 48kHz/stereo output (e.g. many audio interfaces default to
    /// 44100Hz), and writing samples at the wrong rate straight into the output buffer
    /// produces audible noise rather than a clean error.
    pub fn play(&self, track_id: &str, frame: AudioFrame) {
        self.sink().play(track_id, frame);
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

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        let active_now = ACTIVE_PLAYER_COUNT
            .fetch_sub(1, Ordering::Relaxed)
            .saturating_sub(1);
        tracing::info!(
            player_id = self.id,
            active_player_count = active_now,
            total_dropped_frames = self.dropped_frames.load(Ordering::Relaxed),
            "AudioPlayer output stream closed"
        );
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
pub fn resample_interleaved(data: &[f32], channels: u16, from_rate: u32, to_rate: u32) -> Vec<f32> {
    if channels == 0 {
        tracing::warn!(
            requested_channels = 0,
            "clamping anomalous zero-channel resample request"
        );
    }
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
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::as_conversions
        )]
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

/// One remote track's buffered audio, and whether it is currently playing out.
///
/// The flag is the jitter buffer: a track that has not yet accumulated
/// [`JitterConfig::prefill_samples`] contributes silence and keeps what it has, rather
/// than playing a partial buffer and immediately starving. Without it, every packet that
/// arrives even slightly late tears a hole in the output, which is heard as the
/// crunchy, low-bitrate quality of a stream that is in fact being delivered intact.
#[derive(Default)]
pub struct TrackBuffer {
    samples: std::collections::VecDeque<f32>,
    playing: bool,
}

impl TrackBuffer {
    /// Buffer newly-decoded samples, discarding the oldest if the track has run away.
    ///
    /// An over-full buffer is latency, not safety margin: a listener hears every buffered
    /// sample before they hear the speaker's next word. Trimming from the front keeps the
    /// delay bounded at the cost of one audible skip, which is the better trade against
    /// a conversation that drifts permanently behind.
    fn push(&mut self, samples: impl IntoIterator<Item = f32>, config: &JitterConfig) {
        self.samples.extend(samples);
        if self.samples.len() > config.max_samples {
            let excess = self.samples.len().saturating_sub(config.max_samples);
            drop(self.samples.drain(..excess));
        }
    }

    /// Whether this track should contribute audio to the next callback.
    fn ready(&mut self, config: &JitterConfig) -> bool {
        if !self.playing && self.samples.len() >= config.prefill_samples {
            self.playing = true;
        }
        self.playing
    }

    /// Number of buffered samples, for tests and stats.
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether the track holds no audio at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

/// Per-track sample buffers awaiting playback, keyed by track id.
///
/// One entry per remote track. Samples within an entry are strictly sequential in time;
/// separate entries are concurrent with each other.
pub type TrackQueues = std::collections::HashMap<String, TrackBuffer>;

/// How much audio to hold back before playing a track, and how much to hold at most.
///
/// Both are in interleaved samples, so they depend on the output stream's rate and
/// channel count -- 60ms is 5760 samples at 48kHz stereo and 2646 at 44.1kHz mono, and
/// getting that conversion wrong is the difference between a jitter buffer and a
/// quarter-second of latency.
#[derive(Clone, Copy)]
pub struct JitterConfig {
    prefill_samples: usize,
    max_samples: usize,
}

/// Audio held back before a track starts playing.
///
/// Covers ordinary network and scheduling jitter (a 20ms frame arriving up to two frames
/// late) without being long enough to be perceived as delay in conversation.
const PREFILL: std::time::Duration = std::time::Duration::from_millis(60);

/// The most audio a track may hold before the oldest is discarded.
const MAX_BUFFER: std::time::Duration = std::time::Duration::from_millis(240);

impl JitterConfig {
    /// Buffer depths for an output stream running at `sample_rate` with `channels`.
    #[must_use]
    pub fn for_stream(sample_rate: u32, channels: u16) -> Self {
        let per_second = u64::from(sample_rate).saturating_mul(u64::from(channels.max(1)));
        let samples_for = |d: std::time::Duration| -> usize {
            usize::try_from(
                per_second
                    .saturating_mul(u64::from(d.subsec_millis()))
                    .checked_div(1000)
                    .unwrap_or(0),
            )
            .unwrap_or(usize::MAX)
        };
        Self {
            prefill_samples: samples_for(PREFILL),
            max_samples: samples_for(MAX_BUFFER),
        }
    }

    /// Play everything the instant it arrives.
    ///
    /// For tests that are about mixing arithmetic rather than buffering, and for nothing
    /// else: on a real stream this is what produces the underruns the jitter buffer exists
    /// to prevent.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            prefill_samples: 0,
            max_samples: usize::MAX,
        }
    }
}

/// Mix one callback's worth of audio: sequentially *within* each track, additively
/// *across* tracks.
///
/// The distinction is the whole point, and getting it wrong is a real, confirmed
/// "digital screeching" bug in both directions:
///
/// - Treating concurrent tracks as sequential time-division-multiplexes two speakers into
///   alternating ~20ms chunks of each waveform.
/// - Treating sequential frames as concurrent (the bug this version fixes) sums a track's
///   own consecutive 20ms chunks on top of each other at the same instant. Because every
///   superimposed frame also advances by a full callback's worth of samples, the queue
///   then drains at N times the rate it fills, so audio plays at N-times speed with N
///   different points of the same waveform overlaid, followed by starvation and silence.
///   That is what a single-queue mixer with no track identity does to ordinary jitter --
///   and jitter guarantees more than one frame is queued regularly, since the decode
///   thread produces a 20ms frame every 20ms while the output callback consumes ~25ms at
///   a time.
///
/// A track that runs dry contributes silence for the remainder of the buffer rather than
/// stalling the others. Fully-drained tracks are dropped so idle/departed participants
/// don't accumulate.
fn mix_tracks_into(output: &mut [f32], tracks: &mut TrackQueues, config: &JitterConfig) {
    for sample in output.iter_mut() {
        *sample = 0.0;
    }

    for track in tracks.values_mut() {
        if !track.ready(config) {
            continue;
        }
        for sample in output.iter_mut() {
            let Some(value) = track.samples.pop_front() else {
                // Running dry mid-callback means the buffer was too shallow for the
                // jitter actually present. Re-arming the prefill costs one gap now
                // instead of one per callback for as long as the condition lasts.
                track.playing = false;
                break;
            };
            *sample += value;
        }
    }

    for sample in output.iter_mut() {
        *sample = sample.clamp(-1.0, 1.0);
    }

    // Fully-drained tracks are dropped so departed participants don't accumulate. A
    // track that returns later re-prefills, which is the same thing that happens to a
    // track that starves without being dropped.
    tracks.retain(|_, track| !track.samples.is_empty());
}

fn build_output_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    rx: mpsc::Receiver<(String, AudioFrame)>,
) -> Result<Stream, PlaybackError> {
    let err_fn = |err| tracing::error!("Audio playback error: {err}");

    // Buffered samples per remote track. Keyed by track so the mixer can tell a track's
    // own consecutive frames (which must play one after another) apart from different
    // tracks' frames (which must be summed) -- see `mix_tracks_into`.
    let mut tracks: TrackQueues = TrackQueues::new();
    let jitter = JitterConfig::for_stream(config.sample_rate.0, config.channels);

    let stream = device
        .build_output_stream(
            config,
            move |output: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                // Append everything that arrived since the last callback to its own
                // track's queue, preserving arrival order within each track.
                while let Ok((track_id, frame)) = rx.try_recv() {
                    tracks
                        .entry(track_id)
                        .or_default()
                        .push(frame.data, &jitter);
                }
                mix_tracks_into(output, &mut tracks, &jitter);
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

    /// Regression test: a misbehaving capture device reporting `channels = 0` must not
    /// panic (the `channels.max(1)` guard holds), and the anomaly must be visible in
    /// structured logs rather than silently masked forever by the guard.
    #[test]
    #[allow(clippy::expect_used)]
    fn zero_channels_is_clamped_without_panic_and_logged() {
        let capture = elementium_observability_test::LogCapture::new();
        let samples = [0.1_f32, 0.2, 0.3, 0.4];

        let output = capture.run(|| resample_interleaved(&samples, 0, 44100, 48000));

        assert!(!output.is_empty());

        let event = capture
            .find_event("clamping anomalous zero-channel resample request")
            .expect("a structured warning should have been emitted for the zero-channels input");
        assert_eq!(event.field("requested_channels"), Some("0"));
    }

    #[test]
    fn nonzero_channels_does_not_log_the_clamp_warning() {
        let capture = elementium_observability_test::LogCapture::new();
        let samples = [0.1_f32, 0.2, 0.3, 0.4];

        capture.run(|| resample_interleaved(&samples, 2, 44100, 48000));

        assert!(
            capture
                .find_event("clamping anomalous zero-channel resample request")
                .is_none()
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod pick_preferred_config_tests {

    fn range(
        channels: u16,
        min: u32,
        max: u32,
        format: cpal::SampleFormat,
    ) -> cpal::SupportedStreamConfigRange {
        cpal::SupportedStreamConfigRange::new(
            channels,
            cpal::SampleRate(min),
            cpal::SampleRate(max),
            cpal::SupportedBufferSize::Range { min: 0, max: 0 },
            format,
        )
    }

    /// The actual real-world bug this pins: `default_output_config()` biases toward
    /// 44100Hz even when a device also advertises 48000Hz (Opus's native rate, and
    /// commonly the real rate a PipeWire-backed "default" device is running its graph
    /// at). When both are available, explicit 48000 selection must win over whatever
    /// cpal's own default heuristic would have picked.
    #[test]
    fn prefers_exact_48000_over_44100_when_both_available() {
        let configs = vec![
            range(2, 44_100, 44_100, cpal::SampleFormat::F32),
            range(2, 48_000, 48_000, cpal::SampleFormat::F32),
        ];
        let picked = crate::stream_config::pick_preferred_config(configs.into_iter(), 48_000)
            .expect("48000 should be selectable");
        assert_eq!(picked.sample_rate().0, 48_000);
    }

    /// A single range that merely spans 48000 (rather than being pinned to it exactly)
    /// must also be usable -- ALSA/PipeWire commonly advertise a min/max range, not a
    /// discrete list of exact rates.
    #[test]
    fn selects_from_a_range_that_covers_the_preferred_rate() {
        let configs = vec![range(2, 8_000, 96_000, cpal::SampleFormat::F32)];
        let picked = crate::stream_config::pick_preferred_config(configs.into_iter(), 48_000)
            .expect("range covering 48000 should be selectable");
        assert_eq!(picked.sample_rate().0, 48_000);
    }

    /// Device genuinely cannot do 48000 at all -- caller must fall back to the device's
    /// own default rather than picking something arbitrary or panicking.
    #[test]
    fn returns_none_when_device_cannot_do_the_preferred_rate() {
        let configs = vec![range(2, 22_050, 22_050, cpal::SampleFormat::F32)];
        assert!(crate::stream_config::pick_preferred_config(configs.into_iter(), 48_000).is_none());
    }

    /// Among multiple configs that all support 48000, stereo must be preferred over
    /// mono, matching how the rest of this pipeline (decoder, mixer) already assumes
    /// stereo.
    #[test]
    fn prefers_stereo_over_mono_at_the_same_rate() {
        let configs = vec![
            range(1, 48_000, 48_000, cpal::SampleFormat::F32),
            range(2, 48_000, 48_000, cpal::SampleFormat::F32),
        ];
        let picked = crate::stream_config::pick_preferred_config(configs.into_iter(), 48_000)
            .expect("48000 should be selectable");
        assert_eq!(picked.channels(), 2);
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp, clippy::similar_names)]
mod mix_tracks_tests {
    use super::*;

    fn queues(entries: &[(&str, &[f32])]) -> TrackQueues {
        entries
            .iter()
            .map(|(id, samples)| {
                let mut buffer = TrackBuffer::default();
                buffer.push(samples.iter().copied(), &JitterConfig::none());
                ((*id).to_string(), buffer)
            })
            .collect()
    }

    /// Mix with no buffering, for the tests that are about mixing arithmetic.
    fn mix(output: &mut [f32], tracks: &mut TrackQueues) {
        mix_tracks_into(output, tracks, &JitterConfig::none());
    }

    /// A single track plays through unchanged (mixing is a no-op with one source).
    #[test]
    fn single_track_passes_through() {
        let mut tracks = queues(&[("a", &[0.1, 0.2, 0.3, 0.4])]);
        let mut output = [0.0f32; 4];
        mix(&mut output, &mut tracks);
        assert_eq!(output, [0.1, 0.2, 0.3, 0.4]);
    }

    /// THE regression test for the confirmed "digital screeching" root cause.
    ///
    /// One track's consecutive frames must play one after another, never summed on top of
    /// each other. The previous mixer kept a single queue with no track identity, so any
    /// time jitter left two frames of the same track queued (routine: a 20ms frame is
    /// produced every 20ms while the output callback consumes ~25ms at a time) it
    /// superimposed them *and* advanced both by a full buffer -- draining the queue at
    /// double rate, so the track played at 2x speed with two different points of its own
    /// waveform overlaid, then starved into silence.
    #[test]
    fn one_tracks_consecutive_frames_play_sequentially_not_superimposed() {
        let mut tracks = TrackQueues::new();
        // Two consecutive frames of the SAME track, both queued before the callback runs.
        tracks
            .entry("track-a".to_string())
            .or_default()
            .push([0.1, 0.1, 0.5, 0.5], &JitterConfig::none());

        // Callback only has room for the first frame's worth.
        let mut first = [0.0f32; 2];
        mix(&mut first, &mut tracks);
        assert_eq!(
            first,
            [0.1, 0.1],
            "first callback must play only the first frame, not the sum of both"
        );

        // The second frame must still be queued, and play next -- unconsumed and intact.
        let mut second = [0.0f32; 2];
        mix(&mut second, &mut tracks);
        assert_eq!(
            second,
            [0.5, 0.5],
            "second frame must survive to play next, at its own amplitude"
        );
    }

    /// The complementary case: genuinely concurrent tracks must be summed, not alternated.
    #[test]
    fn two_concurrent_tracks_are_summed() {
        let track_a = [0.1, -0.2, 0.3, -0.1];
        let track_b = [0.05, 0.05, -0.1, 0.2];
        let mut tracks = queues(&[("a", &track_a), ("b", &track_b)]);
        let mut output = [0.0f32; 4];
        mix(&mut output, &mut tracks);

        for (i, (got, (a, b))) in output.iter().zip(track_a.iter().zip(&track_b)).enumerate() {
            assert!(
                (got - (a + b)).abs() < 1e-6,
                "sample {i}: got {got}, want {}",
                a + b
            );
        }
    }

    /// Overlapping loud tracks clamp instead of wrapping into distortion.
    #[test]
    fn summed_output_clamps_to_valid_range() {
        let mut tracks = queues(&[("a", &[0.9, -0.9]), ("b", &[0.8, -0.8])]);
        let mut output = [0.0f32; 2];
        mix(&mut output, &mut tracks);
        assert_eq!(output, [1.0, -1.0]);
    }

    /// A track with more buffered than one callback consumes resumes where it left off.
    #[test]
    fn partially_consumed_track_resumes_across_callbacks() {
        let mut tracks = queues(&[("a", &[0.1, 0.2, 0.3, 0.4])]);
        let mut first = [0.0f32; 2];
        mix(&mut first, &mut tracks);
        assert_eq!(first, [0.1, 0.2]);
        assert_eq!(tracks.len(), 1, "track still has samples, must stay queued");

        let mut second = [0.0f32; 2];
        mix(&mut second, &mut tracks);
        assert_eq!(second, [0.3, 0.4]);
        assert!(tracks.is_empty(), "drained track must be dropped");
    }

    /// A track that runs dry mid-buffer contributes silence for the rest rather than
    /// stalling or repeating -- and must not hold back tracks that still have audio.
    #[test]
    fn underrunning_track_yields_silence_without_blocking_others() {
        let mut tracks = queues(&[("short", &[1.0]), ("long", &[0.0, 0.25])]);
        let mut output = [0.0f32; 2];
        mix(&mut output, &mut tracks);
        assert_eq!(output, [1.0, 0.25]);
    }

    /// No queued audio -> silence, not stale samples from the previous callback.
    #[test]
    fn no_tracks_yields_silence() {
        let mut tracks = TrackQueues::new();
        let mut output = [1.0f32, -1.0, 0.5];
        mix(&mut output, &mut tracks);
        assert_eq!(output, [0.0, 0.0, 0.0]);
    }

    /// End-to-end over the real codec: two participants' independently-encoded streams,
    /// decoded through their own decoders and mixed through the exact production path.
    #[test]
    #[allow(clippy::expect_used)]
    fn concurrent_opus_streams_decode_and_mix_without_screech() {
        use elementium_codec::{OpusDecoder, OpusEncoder};

        let sample_rate = 48_000u32;
        let channels = 2u16;
        let frame_samples = 960usize; // 20ms at 48kHz per channel

        let make_sine = |freq: f32| -> Vec<f32> {
            (0..frame_samples)
                .flat_map(|i| {
                    #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
                    let t = i as f32 / sample_rate as f32;
                    let s = (t * freq * std::f32::consts::TAU).sin() * 0.3;
                    [s, s]
                })
                .collect()
        };

        let mut encoder_a = OpusEncoder::new(sample_rate, channels).expect("encoder a");
        let mut encoder_b = OpusEncoder::new(sample_rate, channels).expect("encoder b");
        let frame_a = AudioFrame {
            sample_rate,
            channels,
            data: make_sine(220.0),
            timestamp_us: 0,
        };
        let frame_b = AudioFrame {
            sample_rate,
            channels,
            data: make_sine(440.0),
            timestamp_us: 0,
        };
        let packet_a = encoder_a.encode(&frame_a).expect("encode a");
        let packet_b = encoder_b.encode(&frame_b).expect("encode b");

        // Separate decoders per track, exactly as the playback pipelines key them by mid.
        let mut decoder_a = OpusDecoder::new(sample_rate, channels).expect("decoder a");
        let mut decoder_b = OpusDecoder::new(sample_rate, channels).expect("decoder b");
        let decoded_a = decoder_a
            .decode(&packet_a, frame_samples)
            .expect("decode a");
        let decoded_b = decoder_b
            .decode(&packet_b, frame_samples)
            .expect("decode b");

        let mut tracks = queues(&[("a", &decoded_a.data), ("b", &decoded_b.data)]);
        let mut output = vec![0.0f32; decoded_a.data.len()];
        mix(&mut output, &mut tracks);

        // Neither participant may be dropped or overwritten by the other.
        assert_ne!(output, decoded_a.data);
        assert_ne!(output, decoded_b.data);

        // And the result must be their true sum (Opus is lossy, so allow tolerance).
        let max_diff = output
            .iter()
            .zip(decoded_a.data.iter().zip(&decoded_b.data))
            .map(|(mixed, (a, b))| (mixed - (a + b)).abs())
            .fold(0.0f32, f32::max);
        assert!(
            max_diff < 0.05,
            "mixed output diverged from sum by {max_diff}"
        );
    }
}

#[cfg(test)]
mod shared_sink_tests {
    use super::*;

    /// Regression test for a real, confirmed bug: every call site used to open its own
    /// `AudioPlayer` (one per `PeerConnection`, one per `LiveKit` subscriber), so two
    /// independent cpal output streams wrote to the same physical device concurrently --
    /// a real cause of audio glitching, separate from and in addition to a since-fixed
    /// per-track decoder bug. `shared_sink()` must hand out the *same* stream to every
    /// caller, not a fresh one each time.
    ///
    /// Runs against whatever audio device is actually available in this environment
    /// (real hardware locally, likely none in headless CI) -- either way, both calls
    /// must agree: both `Some` referring to the same stream, or both `None`.
    #[test]
    #[allow(clippy::panic)]
    fn shared_sink_returns_the_same_stream_on_every_call() {
        let first = shared_sink();
        let second = shared_sink();

        match (first, second) {
            (Some(a), Some(b)) => {
                assert!(
                    a.same_stream_as(&b),
                    "shared_sink() must return handles to the same underlying output \
                     stream on every call, not open a new one each time"
                );
            }
            (None, None) => {
                // No output device in this environment -- consistent failure is
                // correct; the singleton property has nothing to assert further.
            }
            (a, b) => panic!(
                "shared_sink() gave inconsistent results across calls: {:?} vs {:?} \
                 (must be deterministic: always Some referring to one stream, or \
                 always None)",
                a.is_some(),
                b.is_some()
            ),
        }
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod jitter_buffer_tests {
    use super::{JitterConfig, TrackBuffer, TrackQueues, mix_tracks_into};

    /// 48kHz stereo, and a callback of 10ms -- close enough to a real stream that the
    /// sample counts mean something.
    const RATE: u32 = 48_000;
    const CHANNELS: u16 = 2;
    const CALLBACK_SAMPLES: usize = 960;
    /// One 20ms Opus frame at [`RATE`]/[`CHANNELS`], interleaved.
    const ONE_FRAME: usize = 1_920;

    fn config() -> JitterConfig {
        JitterConfig::for_stream(RATE, CHANNELS)
    }

    fn track(sample_count: usize) -> TrackQueues {
        let mut buffer = TrackBuffer::default();
        buffer.push(std::iter::repeat_n(0.5_f32, sample_count), &config());
        let mut tracks = TrackQueues::new();
        tracks.insert("a".to_string(), buffer);
        tracks
    }

    /// The whole point: one frame's worth of audio is not enough to start on.
    ///
    /// Playing it immediately produces 20ms of audio followed by a gap until the next
    /// packet arrives, and repeats that for the life of the call. The gaps are what a
    /// listener reports as "low bitrate" or "robotic" on a stream that is in fact being
    /// delivered whole.
    #[test]
    fn a_track_holding_less_than_the_prefill_stays_silent_and_keeps_its_audio() {
        let cfg = config();
        let mut tracks = track(ONE_FRAME);
        let mut output = [1.0_f32; CALLBACK_SAMPLES];

        mix_tracks_into(&mut output, &mut tracks, &cfg);

        assert!(
            output.iter().all(|s| *s == 0.0),
            "a track below the prefill threshold must contribute silence"
        );
        assert_eq!(
            tracks.get("a").map(TrackBuffer::len),
            Some(ONE_FRAME),
            "and must keep every sample it has, not consume them into the gap"
        );
    }

    /// Once enough audio has accumulated, it plays -- and keeps playing across callbacks
    /// without re-prefilling.
    #[test]
    fn a_track_past_the_prefill_plays_and_stays_playing() {
        let cfg = config();
        let mut tracks = track(CALLBACK_SAMPLES * 6);
        let mut first = [0.0_f32; CALLBACK_SAMPLES];
        let mut second = [0.0_f32; CALLBACK_SAMPLES];

        mix_tracks_into(&mut first, &mut tracks, &cfg);
        mix_tracks_into(&mut second, &mut tracks, &cfg);

        assert!(first.iter().all(|s| *s == 0.5), "first callback plays");
        assert!(
            second.iter().all(|s| *s == 0.5),
            "and the second does not stall waiting to prefill again"
        );
    }

    /// A track that runs dry mid-callback re-arms its prefill, so the next callback waits
    /// for a margin instead of tearing a fresh hole every time.
    #[test]
    fn a_starved_track_rearms_its_prefill() {
        let cfg = config();
        let mut tracks = track(CALLBACK_SAMPLES * 6);
        let mut output = [0.0_f32; CALLBACK_SAMPLES];

        for _ in 0..6_u8 {
            mix_tracks_into(&mut output, &mut tracks, &cfg);
        }
        assert!(tracks.is_empty(), "a fully drained track is dropped");

        // One late frame arrives. It must not play on its own.
        tracks = track(ONE_FRAME);
        let mut after = [1.0_f32; CALLBACK_SAMPLES];
        mix_tracks_into(&mut after, &mut tracks, &cfg);
        assert!(
            after.iter().all(|s| *s == 0.0),
            "a single late frame must buffer, not play into a starving stream"
        );
    }

    /// Buffered audio is latency. A track that runs away is trimmed from the front, so a
    /// listener catches up with one skip rather than falling permanently behind.
    #[test]
    fn a_runaway_track_is_trimmed_to_the_maximum() {
        let cfg = config();
        let max = JitterConfig::for_stream(RATE, CHANNELS);
        let mut buffer = TrackBuffer::default();
        buffer.push(std::iter::repeat_n(0.1_f32, 10_000_000), &cfg);

        assert_eq!(
            buffer.len(),
            max.max_samples,
            "the buffer must be bounded by the configured maximum"
        );
    }

    /// The depths must be derived from the stream's real rate and channel count -- the
    /// same duration is a different number of samples on every device.
    #[test]
    fn buffer_depths_scale_with_rate_and_channels() {
        let stereo_48k = JitterConfig::for_stream(48_000, 2);
        let mono_48k = JitterConfig::for_stream(48_000, 1);
        let stereo_44k = JitterConfig::for_stream(44_100, 2);

        assert_eq!(stereo_48k.prefill_samples, 5_760, "60ms at 48kHz stereo");
        assert_eq!(mono_48k.prefill_samples, 2_880, "60ms at 48kHz mono");
        assert_eq!(stereo_44k.prefill_samples, 5_292, "60ms at 44.1kHz stereo");
        assert!(stereo_48k.max_samples > stereo_48k.prefill_samples);
    }
}
