//! Capture PCM audio from a `PipeWire` node -- the monitor of a sink, or an application's
//! own playback stream.
//!
//! This exists because there is no other way to get this audio. The XDG `ScreenCast` portal,
//! which is how screen video is shared, carries no audio in any backend: whichever desktop
//! answers the portal request, the audio stream simply is not there. Share audio can only
//! come from `PipeWire` itself, connected directly to the node whose sound is wanted.
//!
//! `crates/elementium_media/src/audio_capture.rs` is not that path, and extending it was
//! considered and rejected. It captures the microphone through `cpal`, which on Linux goes
//! through ALSA/PulseAudio compatibility rather than talking to `PipeWire` natively -- it
//! cannot address a specific `PipeWire` node at all, whether that node is a sink's monitor or
//! one application's stream among several. Only a direct `PipeWire` client can pick one node
//! out of the graph, which is the entire point of this module.
//!
//! The stream runs on its own thread with its own `PipeWire` main loop and hands samples over
//! a bounded channel, mirroring [`crate::pipewire_capture`]. A full channel drops the newest
//! buffer rather than blocking: a stalled `PipeWire` callback backs up the whole graph, and a
//! dropped buffer is a click, not a stall.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use elementium_types::AudioFrame;

use crate::pipewire_nodes::PipewireError;

/// Buffers held before the oldest is dropped.
///
/// A `PipeWire` audio quantum is a few milliseconds; a queue of 8 is comfortably under
/// 100ms of audio even at a small quantum, which keeps a slow consumer from turning into
/// growing latency instead of an audible (but bounded) glitch.
const AUDIO_QUEUE_DEPTH: usize = 8;

/// The sample rate this capture asks `PipeWire` for.
///
/// Reused from [`crate::stream_config`] rather than a separate constant: it is Opus's
/// native rate, which the microphone path already standardises on for the same reason (see
/// that module's docs), and requesting it here means share audio and microphone audio need
/// no per-source special-casing downstream.
pub const TARGET_SAMPLE_RATE: u32 = crate::stream_config::PREFERRED_RATE;

/// Which relationship the target node has to the audio graph, which decides how the stream
/// must ask to be connected.
///
/// A sink (`Audio/Sink`) does not produce audio for a client to read at all under normal
/// connection rules -- it is the thing audio is sent *to*. Its monitor ports, which mirror
/// what is being played, are only reached by asking `PipeWire` to redirect the connection
/// there. An application stream (`Stream/Output/Audio`) is already a producer with output
/// ports of its own, and connects the ordinary way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSourceKind {
    /// An `Audio/Sink` device. Captures its monitor -- the whole mix being played to it --
    /// rather than failing to connect to a node with no readable output.
    SinkMonitor,
    /// A `Stream/Output/Audio` node: one application's own playback.
    AppStream,
}

/// Negotiated format, shared between the format callback and the process callback.
#[derive(Debug, Clone, Copy, Default)]
struct Negotiated {
    rate: u32,
    channels: u16,
    /// Distinguishes "not negotiated yet" from a source that (implausibly) negotiated 0
    /// channels; buffers are dropped rather than guessed at until this is true.
    known: bool,
}

/// A running `PipeWire` audio capture.
pub struct PipewireAudioCapture {
    frame_rx: mpsc::Receiver<AudioFrame>,
    stop_tx: mpsc::Sender<()>,
    negotiated: Arc<Mutex<Negotiated>>,
    /// Set when the stream enters its error state; see [`crate::pipewire_capture`] for why
    /// this is tracked separately from "no buffers yet".
    failed: Arc<AtomicBool>,
    /// Buffers dropped because the consumer had not taken the previous ones. A plain
    /// counter, not folded into a periodic log line: a caller deciding whether share audio
    /// is healthy needs this number on demand, not only in the logs.
    dropped: Arc<AtomicU64>,
}

impl PipewireAudioCapture {
    /// Connect to `node_id` and start delivering audio, requesting [`TARGET_SAMPLE_RATE`]
    /// interleaved `f32` PCM.
    ///
    /// The channel count is left for the source to decide: a sink monitor is almost always
    /// stereo and an application stream can be either, and there is nothing to gain by
    /// asking for a specific count only to have it resampled or remixed back. Whatever
    /// count is negotiated is reported through [`Self::channels`].
    ///
    /// The sample rate is not left open the same way. `PipeWire` inserts a format-converting
    /// adapter into every audio stream connection (unlike the video path, which talks to the
    /// source's own format directly), so asking for a fixed rate here does not risk the
    /// connection failing the way asking for a fixed pixel format would -- it makes the
    /// adapter resample, once, centrally, instead of every consumer resampling its own copy
    /// of whatever rate the graph happens to run at.
    ///
    /// # Errors
    ///
    /// Returns [`PipewireError`] if `PipeWire` cannot be initialised or the daemon is
    /// unreachable. A node that exists but never produces audio is not an error here -- it
    /// surfaces as no frames arriving, which the caller can time out on.
    pub fn start(node_id: u32, kind: AudioSourceKind) -> Result<Self, PipewireError> {
        let (frame_tx, frame_rx) = mpsc::sync_channel(AUDIO_QUEUE_DEPTH);
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let negotiated = Arc::new(Mutex::new(Negotiated::default()));
        let failed = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicU64::new(0));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

        let thread_negotiated = Arc::clone(&negotiated);
        let thread_failed = Arc::clone(&failed);
        let thread_dropped = Arc::clone(&dropped);
        std::thread::Builder::new()
            .name(format!("pw-audio-{node_id}"))
            .spawn(move || {
                run_stream(
                    node_id,
                    kind,
                    &frame_tx,
                    stop_rx,
                    &thread_negotiated,
                    &thread_failed,
                    &thread_dropped,
                    &ready_tx,
                );
            })
            .map_err(PipewireError::SpawnThread)?;

        match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                frame_rx,
                stop_tx,
                negotiated,
                failed,
                dropped,
            }),
            Ok(Err(reason)) => Err(PipewireError::StreamSetup { reason }),
            Err(e) => Err(PipewireError::StreamSetup {
                reason: format!("audio stream setup timed out: {e}"),
            }),
        }
    }

    /// The most recent audio buffer, if one is waiting.
    #[must_use]
    pub fn try_recv(&self) -> Option<AudioFrame> {
        self.frame_rx.try_recv().ok()
    }

    /// Whether the stream has given up.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// Negotiated `(sample_rate, channels)`, or `(0, 0)` before the format callback has run.
    #[must_use]
    pub fn format(&self) -> (u32, u16) {
        self.negotiated
            .lock()
            .map_or((0, 0), |n| if n.known { (n.rate, n.channels) } else { (0, 0) })
    }

    /// Buffers dropped so far because the consumer was not keeping up.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Stop the capture thread.
    pub fn stop(&self) {
        let _ = self.stop_tx.send(());
    }
}

impl Drop for PipewireAudioCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Body of the capture thread: build the stream, run the loop until stopped.
#[allow(clippy::too_many_arguments)]
fn run_stream(
    node_id: u32,
    kind: AudioSourceKind,
    frame_tx: &mpsc::SyncSender<AudioFrame>,
    stop_rx: mpsc::Receiver<()>,
    negotiated: &Arc<Mutex<Negotiated>>,
    failed: &Arc<AtomicBool>,
    dropped: &Arc<AtomicU64>,
    ready_tx: &mpsc::Sender<Result<(), String>>,
) {
    pipewire::init();

    let setup = || -> Result<_, String> {
        let mainloop = pipewire::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
        let context =
            pipewire::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
        let core = context.connect_rc(None).map_err(|e| e.to_string())?;
        Ok((mainloop, core))
    };

    let (mainloop, core) = match setup() {
        Ok(v) => v,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    let mut props = pipewire::properties::properties! {
        *pipewire::keys::MEDIA_TYPE => "Audio",
        *pipewire::keys::MEDIA_CATEGORY => "Capture",
        // Not "Communication": that role is for the microphone, which policy elsewhere
        // (echo cancellation, ducking) treats specially. This is desktop or application
        // audio being picked up for a call, not a voice source.
        *pipewire::keys::MEDIA_ROLE => "Production",
    };
    if kind == AudioSourceKind::SinkMonitor {
        // Without this, connecting to a sink node asks to read from a node that has no
        // output for a client to read at all -- a sink consumes audio, it does not produce
        // it. This flag is what redirects the connection to the sink's monitor ports, which
        // mirror the mix being played to it.
        props.insert(*pipewire::keys::STREAM_CAPTURE_SINK, "true");
    }

    let stream = match pipewire::stream::StreamRc::new(core, "elementium-audio-capture", props) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(e.to_string()));
            return;
        }
    };

    if let Err(e) = attach_and_connect(&stream, node_id, negotiated, failed, frame_tx, dropped) {
        let _ = ready_tx.send(Err(e));
        return;
    }
    let _ = ready_tx.send(Ok(()));
    run_until_stopped(&mainloop, stop_rx, node_id);
}

/// Attach the stream callbacks and connect to `node_id`.
///
/// Returns the listener, which must outlive the stream: dropping it silently stops every
/// callback, so the stream would connect and then deliver nothing. See
/// [`crate::pipewire_capture::attach_and_connect`] for the same reasoning on the video path.
fn attach_and_connect(
    stream: &pipewire::stream::StreamRc,
    node_id: u32,
    negotiated: &Arc<Mutex<Negotiated>>,
    failed: &Arc<AtomicBool>,
    frame_tx: &mpsc::SyncSender<AudioFrame>,
    dropped: &Arc<AtomicU64>,
) -> Result<(), String> {
    let fmt_state = Arc::clone(negotiated);
    let frame_state = Arc::clone(negotiated);
    let tx = frame_tx.clone();
    let drop_counter = Arc::clone(dropped);
    let mut diagnostics = AudioDiagnostics::new(node_id);

    let listener = stream
        .add_local_listener::<()>()
        .state_changed({
            let failed = Arc::clone(failed);
            move |_, (), old, new| {
                tracing::info!(?old, ?new, "PipeWire audio capture stream state");
                if let pipewire::stream::StreamState::Error(reason) = &new {
                    tracing::warn!(
                        reason = %reason,
                        "PipeWire audio capture stream failed"
                    );
                    failed.store(true, Ordering::Relaxed);
                }
            }
        })
        .param_changed(move |_, (), id, pod| {
            if id != libspa::param::ParamType::Format.as_raw() {
                return;
            }
            let Some(pod) = pod else { return };
            handle_format_changed(pod, &fmt_state);
        })
        .process(move |stream, ()| {
            process_buffer(stream, &frame_state, &tx, &drop_counter, &mut diagnostics);
        })
        .register();

    let mut params = [audio_format_param()];
    let mut param_refs: Vec<&libspa::pod::Pod> = params
        .iter_mut()
        .filter_map(|p| libspa::pod::Pod::from_bytes(p))
        .collect();
    if param_refs.is_empty() {
        return Err("could not build the audio format parameters".to_owned());
    }

    stream
        .connect(
            libspa::utils::Direction::Input,
            Some(node_id),
            pipewire::stream::StreamFlags::AUTOCONNECT | pipewire::stream::StreamFlags::MAP_BUFFERS,
            &mut param_refs,
        )
        .map_err(|e| e.to_string())?;

    // Deliberately leaked: see the video path for why this must outlive the function.
    std::mem::forget(listener);
    Ok(())
}

/// Parse a negotiated `Format` param and record it, logging once per change.
fn handle_format_changed(pod: &libspa::pod::Pod, fmt_state: &Arc<Mutex<Negotiated>>) {
    let mut info = libspa::param::audio::AudioInfoRaw::default();
    if info.parse(pod).is_err() {
        tracing::warn!("PipeWire sent an audio format we could not parse");
        return;
    }
    let Ok(channels) = u16::try_from(info.channels()) else {
        tracing::warn!(
            channels = info.channels(),
            "PipeWire negotiated an implausible channel count"
        );
        return;
    };
    tracing::info!(
        rate = info.rate(),
        channels,
        format = ?info.format(),
        "PipeWire audio capture format negotiated"
    );
    if let Ok(mut n) = fmt_state.lock() {
        n.rate = info.rate();
        n.channels = channels;
        n.known = true;
    }
}

/// Rolling counts of what happened to captured buffers, reported periodically rather than
/// per buffer -- a buffer arrives every few milliseconds, and logging that often would cost
/// more than the capture itself.
#[derive(Default)]
struct AudioDiagnostics {
    node_id: u32,
    offered: u64,
    delivered: u64,
    dropped_queue_full: u64,
    dropped_not_negotiated: u64,
    dropped_unusable: u64,
}

impl AudioDiagnostics {
    /// Buffers between reports. `PipeWire`'s default quantum is around 10-20ms, so this is
    /// roughly every 10-20 seconds -- rare enough that the logging is not itself a cost.
    const REPORT_EVERY: u64 = 1000;

    const fn new(node_id: u32) -> Self {
        Self {
            node_id,
            offered: 0,
            delivered: 0,
            dropped_queue_full: 0,
            dropped_not_negotiated: 0,
            dropped_unusable: 0,
        }
    }

    fn report_if_due(&mut self, total_dropped: u64) {
        self.offered = self.offered.saturating_add(1);
        if !self.offered.is_multiple_of(Self::REPORT_EVERY) {
            return;
        }
        tracing::info!(
            node_id = self.node_id,
            offered = self.offered,
            delivered = self.delivered,
            dropped_queue_full = self.dropped_queue_full,
            dropped_not_negotiated = self.dropped_not_negotiated,
            dropped_unusable = self.dropped_unusable,
            total_dropped,
            "PipeWire audio capture buffer accounting"
        );
        let node_id = self.node_id;
        *self = Self::new(node_id);
    }
}

/// The process callback body: pull one buffer, convert it, hand it to the channel.
///
/// Never logs sample data -- only counts and the negotiated shape, matching the rest of
/// this crate's diagnostics.
fn process_buffer(
    stream: &pipewire::stream::Stream,
    frame_state: &Arc<Mutex<Negotiated>>,
    tx: &mpsc::SyncSender<AudioFrame>,
    drop_counter: &Arc<AtomicU64>,
    diagnostics: &mut AudioDiagnostics,
) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let Ok(n) = frame_state.lock().map(|g| *g) else {
        return;
    };
    diagnostics.report_if_due(drop_counter.load(Ordering::Relaxed));
    if !n.known || n.channels == 0 {
        diagnostics.dropped_not_negotiated = diagnostics.dropped_not_negotiated.saturating_add(1);
        return;
    }

    let datas = buffer.datas_mut();
    let Some(data) = datas.first_mut() else { return };
    let size = usize::try_from(data.chunk().size()).unwrap_or(0);
    let Some(mapped) = data.data() else { return };
    let bytes = mapped.get(..size.min(mapped.len())).unwrap_or(mapped);

    let Some(samples) = bytes_to_f32_samples(bytes) else {
        diagnostics.dropped_unusable = diagnostics.dropped_unusable.saturating_add(1);
        tracing::warn!(
            len = bytes.len(),
            "PipeWire audio buffer length was not a whole number of f32 samples; dropped"
        );
        return;
    };

    let frame = AudioFrame {
        sample_rate: n.rate,
        channels: n.channels,
        data: samples,
        timestamp_us: 0,
    };
    if tx.try_send(frame).is_err() {
        diagnostics.dropped_queue_full = diagnostics.dropped_queue_full.saturating_add(1);
        drop_counter.fetch_add(1, Ordering::Relaxed);
    } else {
        diagnostics.delivered = diagnostics.delivered.saturating_add(1);
    }
}

/// Decode a raw little-endian `f32` PCM buffer into samples.
///
/// Requested format is always `F32LE` (see [`audio_format_param`]), so this is the only
/// layout the process callback ever needs to read. Returns `None` if the buffer length is
/// not a whole number of 4-byte samples, which would otherwise silently drop a trailing
/// partial sample.
fn bytes_to_f32_samples(bytes: &[u8]) -> Option<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    bytes
        .chunks_exact(4)
        .map(|c| Some(f32::from_le_bytes(c.try_into().ok()?)))
        .collect()
}

/// Run the main loop until a stop is requested.
fn run_until_stopped(
    mainloop: &pipewire::main_loop::MainLoopRc,
    stop_rx: mpsc::Receiver<()>,
    node_id: u32,
) {
    let quit_loop = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop_rx.try_recv().is_ok() {
            quit_loop.quit();
        }
    });
    if let Err(e) = timer
        .update_timer(
            Some(std::time::Duration::from_millis(100)),
            Some(std::time::Duration::from_millis(100)),
        )
        .into_result()
    {
        // Same failure mode as the video capture loop's identically-shaped timer (see
        // `pipewire_capture.rs::run_until_stopped`): if this never fires, `stop()` is queued
        // and never read, so the mainloop -- and this thread -- outlives every caller that
        // has given up on it, with nothing in the log to say the stop-poll was never armed.
        tracing::error!(
            node_id,
            error = %e,
            "failed to arm the stop-poll timer; stop() may never be noticed"
        );
    }

    tracing::info!(node_id, "PipeWire audio capture loop running");
    mainloop.run();
    tracing::info!(node_id, "PipeWire audio capture loop stopped");
}

/// Build the single `Format` parameter offered: raw interleaved `f32` PCM at
/// [`TARGET_SAMPLE_RATE`], any channel count.
///
/// `PipeWire` audio streams carry an internal converting adapter (see [`PipewireAudioCapture::start`]),
/// so asking for one exact rate here is a request the adapter satisfies by resampling, not a
/// constraint that can fail the connection the way an unavailable pixel format would on the
/// video path.
fn audio_format_param() -> Vec<u8> {
    let mut info = libspa::param::audio::AudioInfoRaw::new();
    info.set_format(libspa::param::audio::AudioFormat::F32LE);
    info.set_rate(TARGET_SAMPLE_RATE);
    let obj = libspa::pod::Object {
        type_: libspa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: libspa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    libspa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &libspa::pod::Value::Object(obj),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::bytes_to_f32_samples;

    #[test]
    fn decodes_little_endian_f32_samples() {
        let bytes = 1.5_f32
            .to_le_bytes()
            .into_iter()
            .chain((-2.25_f32).to_le_bytes())
            .collect::<Vec<u8>>();
        assert_eq!(bytes_to_f32_samples(&bytes), Some(vec![1.5, -2.25]));
    }

    #[test]
    fn refuses_a_length_that_is_not_a_whole_number_of_samples() {
        assert_eq!(bytes_to_f32_samples(&[0, 0, 0]), None);
        assert_eq!(bytes_to_f32_samples(&[0, 0, 0, 0, 1]), None);
    }

    #[test]
    fn empty_input_decodes_to_no_samples() {
        assert_eq!(bytes_to_f32_samples(&[]), Some(Vec::new()));
    }
}
