use std::sync::{Arc, Mutex};

use tauri::{State, command};
use tokio::sync::mpsc as tokio_mpsc;

use elementium_codec::Vp8Encoder;
use elementium_media::camera::CameraCapturer;
use elementium_media::device_enumeration;
use elementium_types::{MediaConstraints, MediaDevice, TrackId, VideoFrame};
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

/// State for active media tracks (audio capture, video capture, etc.).
pub struct MediaState {
    pub active_tracks: Mutex<Vec<TrackId>>,
    /// Active camera pipeline (at most one camera at a time).
    pub camera: Mutex<Option<CameraPipelineHandle>>,
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

        // Stop any existing camera pipeline
        if let Ok(mut cam) = media_state.camera.lock() {
            if let Some(old) = cam.take() {
                let _ = old.stop_tx.send(());
            }
        }

        let req_width = video_constraints.width;
        let req_height = video_constraints.height;

        let encode_tx: Arc<Mutex<Option<tokio_mpsc::Sender<IoCommand>>>> =
            Arc::new(Mutex::new(None));
        let encode_tx_clone = encode_tx.clone();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let tid = track_id.0.clone();

        // Start the camera pipeline on a background thread
        std::thread::spawn(move || {
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

fn generate_track_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{t:x}")
}
