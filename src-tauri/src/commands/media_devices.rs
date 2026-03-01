use std::sync::{Arc, Mutex};

use tauri::{State, command};
use tokio::sync::mpsc as tokio_mpsc;

use elementium_codec::{OpusEncoder, Vp8Encoder};
use elementium_media::audio_capture::AudioCapturer;
use elementium_media::camera::CameraCapturer;
use elementium_media::device_enumeration;
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
                label: cam.human_name().to_string(),
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
    tracing::info!(?constraints, "getUserMedia request");
    let mut track_ids = Vec::new();

    if constraints.audio.is_some() {
        let track_id = TrackId(format!("audio-{}", generate_track_id()));
        tracing::info!(track_id = %track_id, "Starting audio capture");

        // Stop any existing audio capture pipeline
        if let Ok(mut audio) = media_state.audio_capture.lock() {
            if let Some(old) = audio.take() {
                let _ = old.stop_tx.send(());
            }
        }

        let encode_tx: Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>> =
            Arc::new(Mutex::new(None));
        let encode_tx_clone = encode_tx.clone();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();

        // Start audio capture pipeline on a background thread
        std::thread::spawn(move || {
            audio_capture_loop(encode_tx_clone, stop_rx);
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
        let had_previous = {
            let mut had = false;
            if let Ok(mut cam) = media_state.camera.lock() {
                if let Some(old) = cam.take() {
                    let _ = old.stop_tx.send(());
                    had = true;
                }
            }
            had
        };

        let req_width = video_constraints.width;
        let req_height = video_constraints.height;

        let encode_tx: Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>> =
            Arc::new(Mutex::new(None));
        let encode_tx_clone = encode_tx.clone();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let tid = track_id.0.clone();

        // Start the camera pipeline on a background thread.
        // If we just stopped a previous pipeline, delay to let the V4L2
        // device release (avoids EBUSY on Linux).
        std::thread::spawn(move || {
            if had_previous {
                tracing::info!("Waiting for previous camera to release device...");
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            camera_pipeline_loop(tid, video_frames, encode_tx_clone, stop_rx, req_width, req_height);
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
    if track_id.0.starts_with("video-") {
        if let Ok(mut cam) = media_state.camera.lock() {
            if let Some(ref handle) = *cam {
                if handle.track_id == track_id.0 {
                    let _ = handle.stop_tx.send(());
                    *cam = None;
                }
            }
        }
    }

    // If this is an audio track, stop the audio capture pipeline
    if track_id.0.starts_with("audio-") {
        if let Ok(mut audio) = media_state.audio_capture.lock() {
            if let Some(ref handle) = *audio {
                if handle.track_id == track_id.0 {
                    let _ = handle.stop_tx.send(());
                    *audio = None;
                }
            }
        }
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
#[command]
pub fn get_video_frame(
    state: State<'_, VideoFrameState>,
    track_id: String,
) -> tauri::ipc::Response {
    let frame = state.0.lock().ok().and_then(|f| f.get(&track_id).cloned());

    static CALL_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = CALL_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if count < 3 || count % 300 == 0 {
        tracing::info!(
            track_id = %track_id,
            has_frame = frame.is_some(),
            count,
            "get_video_frame IPC call"
        );
    }

    match frame {
        Some(f) => {
            let mut body = Vec::with_capacity(8 + f.data.len());
            body.extend_from_slice(&f.width.to_le_bytes());
            body.extend_from_slice(&f.height.to_le_bytes());
            body.extend_from_slice(&f.data);
            tauri::ipc::Response::new(body)
        }
        None => tauri::ipc::Response::new(vec![0u8; 8]),
    }
}

/// Background thread: reads camera frames, writes RGBA to VideoFrameBuffer for
/// preview, and optionally VP8-encodes + sends to a peer connection.
fn camera_pipeline_loop(
    track_id: String,
    video_frames: VideoFrameBuffer,
    encode_tx: Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
    stop_rx: std::sync::mpsc::Receiver<()>,
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
                buf.remove(&track_id);
            }
            break;
        }

        if let Some(frame) = capturer.try_recv() {
            frame_count += 1;
            if frame_count <= 3 || frame_count % 100 == 0 {
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
                    track_id.clone(),
                    VideoFrame {
                        width: frame.width,
                        height: frame.height,
                        data: frame.data.clone(),
                        timestamp_us: 0,
                    },
                );
            }

            // VP8 encode and send if encoding is active
            let should_encode = encode_tx
                .lock()
                .map(|g| g.is_some())
                .unwrap_or(false);

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
                            if let Ok(guard) = encode_tx.lock() {
                                if let Some(ref tx) = *guard {
                                    for packet in packets {
                                        let _ = tx.try_send(IoCommand::WriteVideo(packet.data));
                                    }
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
    encode_tx: Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>>,
    stop_rx: std::sync::mpsc::Receiver<()>,
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
        44100 => 48000,
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

    // Opus frame size: 20ms at the given sample rate
    let frame_samples = (opus_rate as usize * 20) / 1000;
    let frame_total_samples = frame_samples * channels.min(2) as usize;
    let mut accumulator: Vec<f32> = Vec::with_capacity(frame_total_samples * 2);

    loop {
        if stop_rx.try_recv().is_ok() {
            tracing::info!("Audio capture stopping");
            break;
        }

        if let Some(frame) = capturer.try_recv() {
            let mut data = frame.data;

            // Simple sample rate conversion for 44.1kHz → 48kHz
            if sample_rate == 44100 && opus_rate == 48000 {
                data = resample_44100_to_48000(&data, channels as usize);
            }

            accumulator.extend_from_slice(&data);

            // Process complete Opus frames
            while accumulator.len() >= frame_total_samples {
                let frame_data: Vec<f32> = accumulator.drain(..frame_total_samples).collect();

                // Only encode and send if connected to a peer connection
                let should_encode = encode_tx
                    .lock()
                    .map(|g| g.is_some())
                    .unwrap_or(false);

                if should_encode {
                    let audio_frame = AudioFrame {
                        sample_rate: opus_rate,
                        channels: channels.min(2),
                        data: frame_data,
                        timestamp_us: 0,
                    };

                    match encoder.encode(&audio_frame) {
                        Ok(encoded) => {
                            if let Ok(guard) = encode_tx.lock() {
                                if let Some(ref tx) = *guard {
                                    let _ = tx.try_send(IoCommand::WriteAudio(encoded));
                                }
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

fn generate_track_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{t:x}")
}
