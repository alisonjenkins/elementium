// Every `#[tauri::command]` async fn below that takes a `State<'_, T>` parameter causes
// the `#[command]` macro to generate a sibling IPC-dispatch wrapper item in this module
// containing an internal match with an arm clippy flags as unreachable. That wrapper is
// framework codegen (not nested inside the fn item itself, so a function- or
// statement-scoped `#[allow]` cannot reach it — verified empirically), hence the
// module-level allow here rather than the usual per-item scoping.
#![allow(clippy::unreachable)]
use std::sync::{Arc, Mutex};

use tauri::{State, command};
use tokio::sync::mpsc as tokio_mpsc;

use elementium_codec::{OpusEncoder, Vp8Encoder};
use elementium_media::audio_capture::AudioCapturer;
use elementium_media::camera::CameraCapturer;
use elementium_media::device_enumeration;
use elementium_types::observability::CorrelationId;
use elementium_types::{AudioFrame, MediaConstraints, MediaDevice, TrackId, VideoFrame};
use elementium_webrtc::engine::{IoCommand, VideoFrameBuffer};

use super::webrtc::WebRtcState;
use crate::protocols::VideoFrameState;

/// Handle to a running camera pipeline.
pub struct CameraPipelineHandle {
    pub track_id: String,
    pub stop_tx: std::sync::mpsc::Sender<()>,
    /// Set to enable VP8 encoding and sending to a peer connection.
    /// When `None`, the pipeline only writes RGBA frames for preview.
    pub encode_tx: Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
}

/// Handle to a running audio capture pipeline.
pub struct AudioCaptureHandle {
    pub track_id: String,
    pub stop_tx: std::sync::mpsc::Sender<()>,
    /// Set to enable Opus encoding and sending to a peer connection.
    /// When `None`, the pipeline captures but doesn't encode/send.
    pub encode_tx: Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
}

/// State for active media tracks (audio capture, video capture, etc.).
pub struct MediaState {
    pub active_tracks: Mutex<Vec<TrackId>>,
    /// Active camera pipeline (at most one camera at a time).
    pub camera: Mutex<Option<CameraPipelineHandle>>,
    /// Active audio capture pipeline (at most one mic at a time).
    pub audio_capture: Mutex<Option<AudioCaptureHandle>>,
}

#[command]
pub async fn enumerate_devices() -> Result<Vec<MediaDevice>, String> {
    tracing::info!("Enumerating media devices");

    let mut devices = device_enumeration::enumerate_audio_devices();

    // Add video input devices
    // nokhwa device enumeration is best-effort
    if let Ok(cameras) = nokhwa::query(nokhwa::utils::ApiBackend::Auto) {
        for (i, cam) in cameras.iter().enumerate() {
            devices.push(MediaDevice {
                id: format!("video-input-{i}"),
                label: cam.human_name().clone(),
                kind: elementium_types::MediaDeviceKind::VideoInput,
            });
        }
    }

    Ok(devices)
}

#[command]
pub async fn get_user_media(
    webrtc_state: State<'_, WebRtcState>,
    media_state: State<'_, MediaState>,
    constraints: MediaConstraints,
) -> Result<Vec<TrackId>, String> {
    let call_id = CorrelationId::new();
    let call_span = tracing::info_span!(
        "call",
        correlation_id = %call_id,
        audio_requested = constraints.audio.is_some(),
        video_requested = constraints.video.is_some(),
    );
    let _call_guard = call_span.enter();

    tracing::info!(?constraints, "getUserMedia request");
    let mut track_ids = Vec::new();

    if constraints.audio.is_some() {
        let track_id = TrackId(format!("audio-{}", generate_track_id()));
        tracing::info!(track_id = %track_id, "Starting audio capture");

        // Stop any existing audio capture pipeline
        if let Ok(mut audio) = media_state.audio_capture.lock()
            && let Some(old) = audio.take()
        {
            let _ = old.stop_tx.send(());
        }

        let encode_tx: Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>> =
            Arc::new(Mutex::new(None));
        let encode_tx_clone = encode_tx.clone();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();

        // Start audio capture pipeline on a background thread, inheriting the
        // call's correlation span so every event it emits carries the same
        // correlation_id.
        let audio_span = tracing::Span::current();
        std::thread::spawn(move || {
            let _guard = audio_span.enter();
            audio_capture_loop(&encode_tx_clone, &stop_rx);
        });

        // Store the audio capture handle
        if let Ok(mut audio) = media_state.audio_capture.lock() {
            *audio = Some(AudioCaptureHandle {
                track_id: track_id.0.clone(),
                stop_tx,
                encode_tx,
            });
        }

        if let Ok(mut tracks) = media_state.active_tracks.lock() {
            tracks.push(track_id.clone());
        }
        track_ids.push(track_id);
    }

    if let Some(ref video_constraints) = constraints.video {
        let track_id = TrackId(format!("video-{}", generate_track_id()));
        tracing::info!(track_id = %track_id, "Starting video capture");

        // Get the shared video frame buffer from the WebRTC engine
        let video_frames = {
            let engine = webrtc_state.0.lock().map_err(|e| e.to_string())?;
            engine.video_frames.clone()
        };

        // Stop any existing camera pipeline and wait for the device to release
        let had_previous = if let Ok(mut cam) = media_state.camera.lock()
            && let Some(old) = cam.take()
        {
            let _ = old.stop_tx.send(());
            true
        } else {
            false
        };

        let req_width = video_constraints.width;
        let req_height = video_constraints.height;

        let encode_tx: Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>> =
            Arc::new(Mutex::new(None));
        let encode_tx_clone = encode_tx.clone();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let tid = track_id.0.clone();

        // Start the camera pipeline on a background thread, inheriting the
        // call's correlation span so every event it emits carries the same
        // correlation_id.
        // If we just stopped a previous pipeline, delay to let the V4L2
        // device release (avoids EBUSY on Linux).
        let camera_span = tracing::Span::current();
        std::thread::spawn(move || {
            let _guard = camera_span.enter();
            if had_previous {
                tracing::info!("Waiting for previous camera to release device...");
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            camera_pipeline_loop(
                &tid,
                &video_frames,
                &encode_tx_clone,
                &stop_rx,
                req_width,
                req_height,
            );
        });

        // Store the camera pipeline handle
        if let Ok(mut cam) = media_state.camera.lock() {
            *cam = Some(CameraPipelineHandle {
                track_id: track_id.0.clone(),
                stop_tx,
                encode_tx,
            });
        }

        if let Ok(mut tracks) = media_state.active_tracks.lock() {
            tracks.push(track_id.clone());
        }
        track_ids.push(track_id);
    }

    Ok(track_ids)
}

#[command]
pub async fn stop_track(
    media_state: State<'_, MediaState>,
    track_id: TrackId,
) -> Result<(), String> {
    tracing::info!(%track_id, "Stopping track");

    // If this is the camera track, stop the pipeline
    if track_id.0.starts_with("video-")
        && let Ok(mut cam) = media_state.camera.lock()
        && let Some(ref handle) = *cam
        && handle.track_id == track_id.0
    {
        let _ = handle.stop_tx.send(());
        *cam = None;
    }

    // If this is an audio track, stop the audio capture pipeline
    if track_id.0.starts_with("audio-")
        && let Ok(mut audio) = media_state.audio_capture.lock()
        && let Some(ref handle) = *audio
        && handle.track_id == track_id.0
    {
        let _ = handle.stop_tx.send(());
        *audio = None;
    }

    if let Ok(mut tracks) = media_state.active_tracks.lock() {
        tracks.retain(|t| t != &track_id);
    }
    Ok(())
}

/// Fetch the latest video frame for a track as raw bytes via IPC.
///
/// Returns an 8-byte header (width: u32 LE, height: u32 LE) followed by RGBA data.
/// Returns an 8-byte zero header when no frame is available.
// `state` and `track_id` are only borrowed internally, but tauri's IPC command
// extractors require owned `State<'_, T>` and `String` parameters (the latter must be
// owned because it's deserialized from the IPC payload) — the signature can't be
// changed to take references without breaking command registration.
#[allow(clippy::needless_pass_by_value)]
#[command]
pub fn get_video_frame(
    state: State<'_, VideoFrameState>,
    track_id: String,
) -> tauri::ipc::Response {
    static CALL_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let frame = state.0.lock().ok().and_then(|f| f.get(&track_id).cloned());

    let count = CALL_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if count < 3 || count.is_multiple_of(300) {
        tracing::info!(
            track_id = %track_id,
            has_frame = frame.is_some(),
            count,
            "get_video_frame IPC call"
        );
    }

    match frame {
        Some(f) => {
            let mut body = Vec::with_capacity(f.data.len().saturating_add(8));
            body.extend_from_slice(&f.width.to_le_bytes());
            body.extend_from_slice(&f.height.to_le_bytes());
            body.extend_from_slice(&f.data);
            tauri::ipc::Response::new(body)
        }
        None => tauri::ipc::Response::new(vec![0u8; 8]),
    }
}

/// Background thread: reads camera frames, writes RGBA to `VideoFrameBuffer` for
/// preview, and optionally VP8-encodes + sends to a peer connection.
fn camera_pipeline_loop(
    track_id: &str,
    video_frames: &VideoFrameBuffer,
    encode_tx: &Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
    stop_rx: &std::sync::mpsc::Receiver<()>,
    req_width: Option<u32>,
    req_height: Option<u32>,
) {
    let capturer = match CameraCapturer::start(req_width, req_height) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to start camera: {e}");
            return;
        }
    };

    let width = capturer.width();
    let height = capturer.height();
    tracing::info!(width, height, track_id = %track_id, "Camera pipeline started");

    let mut encoder: Option<Vp8Encoder> = None;
    let mut frame_count: u64 = 0;

    loop {
        if stop_rx.try_recv().is_ok() {
            tracing::info!(track_id = %track_id, "Camera pipeline stopping");
            // Clean up the frame buffer entry
            if let Ok(mut buf) = video_frames.lock() {
                buf.remove(track_id);
            }
            break;
        }

        if let Some(frame) = capturer.try_recv() {
            frame_count = frame_count.wrapping_add(1);
            if frame_count <= 3 || frame_count.is_multiple_of(100) {
                tracing::info!(
                    track_id = %track_id,
                    frame_count,
                    w = frame.width,
                    h = frame.height,
                    data_len = frame.data.len(),
                    "Camera frame received"
                );
            }
            // Write RGBA frame to VideoFrameBuffer for local preview
            if let Ok(mut buf) = video_frames.lock() {
                buf.insert(
                    track_id.to_string(),
                    VideoFrame {
                        width: frame.width,
                        height: frame.height,
                        data: frame.data.clone(),
                        timestamp_us: 0,
                    },
                );
            }

            // VP8 encode and send if encoding is active
            let should_encode = encode_tx.lock().is_ok_and(|g| g.is_some());

            if should_encode {
                // Lazily create the encoder
                if encoder.is_none() {
                    match Vp8Encoder::new(width, height, 500) {
                        Ok(enc) => {
                            tracing::info!(width, height, "VP8 encoder created for camera");
                            encoder = Some(enc);
                        }
                        Err(e) => {
                            tracing::error!("Failed to create VP8 encoder: {e}");
                        }
                    }
                }

                if let Some(ref mut enc) = encoder {
                    let i420 =
                        elementium_codec::rgba_to_i420(frame.width, frame.height, &frame.data);

                    match enc.encode(&i420) {
                        Ok(packets) => {
                            if let Ok(guard) = encode_tx.lock()
                                && let Some(ref tx) = *guard
                            {
                                for packet in packets {
                                    let _ = tx.try_send(IoCommand::WriteVideo(packet.data));
                                }
                            }
                        }
                        Err(e) => {
                            tracing::debug!("VP8 encode error: {e}");
                        }
                    }
                }
            }
        } else {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

/// Background thread: captures mic audio, Opus-encodes, and sends to a peer
/// connection when `encode_tx` is connected (deferred connection pattern).
fn audio_capture_loop(
    encode_tx: &Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
    stop_rx: &std::sync::mpsc::Receiver<()>,
) {
    let capturer = match AudioCapturer::start() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to start audio capture: {e}");
            return;
        }
    };

    let sample_rate = capturer.sample_rate();
    let channels = capturer.channels();

    // Opus supports 8/12/16/24/48kHz; resample 44.1k → 48k
    let opus_rate = match sample_rate {
        8000 | 12000 | 16000 | 24000 | 48000 => sample_rate,
        _ => 48000,
    };

    let mut encoder = match OpusEncoder::new(opus_rate, channels.min(2)) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("Failed to create Opus encoder: {e}");
            return;
        }
    };

    tracing::info!(sample_rate, channels, opus_rate, "Audio capture started");

    // Opus frame size: 20ms at the given sample rate. `opus_rate` is always one of the
    // small fixed constants above, so these conversions/multiplications cannot overflow
    // or lose precision in practice.
    let channels_usize = usize::from(channels.min(2));
    let frame_samples = usize::try_from(opus_rate)
        .unwrap_or(48_000)
        .saturating_mul(20)
        / 1000;
    let frame_total_samples = frame_samples.saturating_mul(channels_usize);
    let mut accumulator: Vec<f32> = Vec::with_capacity(frame_total_samples.saturating_mul(2));

    loop {
        if stop_rx.try_recv().is_ok() {
            tracing::info!("Audio capture stopping");
            break;
        }

        if let Some(frame) = capturer.try_recv() {
            let mut data = frame.data;

            // Simple sample rate conversion for 44.1kHz → 48kHz
            if sample_rate == 44100 && opus_rate == 48000 {
                data = resample_44100_to_48000(&data, usize::from(channels));
            }

            accumulator.extend_from_slice(&data);

            // Process complete Opus frames
            while accumulator.len() >= frame_total_samples {
                let frame_data: Vec<f32> = accumulator.drain(..frame_total_samples).collect();

                // Only encode and send if connected to a peer connection
                let should_encode = encode_tx.lock().is_ok_and(|g| g.is_some());

                if should_encode {
                    let audio_frame = AudioFrame {
                        sample_rate: opus_rate,
                        channels: channels.min(2),
                        data: frame_data,
                        timestamp_us: 0,
                    };

                    match encoder.encode(&audio_frame) {
                        Ok(encoded_frame) => {
                            if let Ok(guard) = encode_tx.lock()
                                && let Some(ref tx) = *guard
                            {
                                let _ = tx.try_send(IoCommand::WriteAudio(encoded_frame));
                            }
                        }
                        Err(e) => {
                            tracing::debug!("Opus encode error: {e}");
                        }
                    }
                }
            }
        } else {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

/// Simple linear interpolation resampling from 44100 to 48000 Hz.
///
/// Sample counts here are bounded by a single ~20ms audio frame (at most a few
/// thousand samples), so the `usize`<->`f64`/`f32` conversions below never approach
/// either type's precision limits; the casts are inherent to the interpolation math,
/// not something `try_from` can meaningfully replace.
#[allow(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::arithmetic_side_effects
)]
fn resample_44100_to_48000(samples: &[f32], channels: usize) -> Vec<f32> {
    // Guard against a misbehaving capture device reporting zero channels.
    let channels = channels.max(1);
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
            output.push((s1 - s0).mul_add(frac, s0));
        }
    }

    output
}

fn generate_track_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{t:x}")
}
