//! Video pipeline: camera/screen → encode → str0m → decode → frame buffer
//!
//! This module wires together:
//! - Camera capture (via elementium-media) or screen capture frames
//! - VP8 encoding of captured frames
//! - Feeding encoded video into str0m peer connections
//! - Receiving encoded video from str0m
//! - VP8 decoding of received video
//! - Writing decoded frames to a shared buffer for the webview

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use elementium_codec::{Vp8Decoder, Vp8Encoder};
use elementium_types::VideoFrame;

use crate::engine::{IoCommand, VideoFrameBuffer};
use crate::peer_connection::PcEvent;

/// Manages the video pipeline for a call session.
pub struct VideoPipeline {
    /// Channel to stop the capture loop.
    stop_tx: Option<mpsc::Sender<()>>,
    /// Whether capture is currently active.
    capture_active: bool,
    /// Whether playback (decode) is active.
    playback_active: bool,
}

impl VideoPipeline {
    pub fn new() -> Self {
        Self {
            stop_tx: None,
            capture_active: false,
            playback_active: false,
        }
    }

    /// Start the camera capture pipeline: camera → VP8 → peer connection.
    pub fn start_camera_capture(
        &mut self,
        io_cmd_tx: mpsc::Sender<IoCommand>,
    ) -> Result<(), String> {
        if self.capture_active {
            return Ok(());
        }

        let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        self.stop_tx = Some(stop_tx);
        self.capture_active = true;

        std::thread::spawn(move || {
            let capturer = match elementium_media::camera::CameraCapturer::start() {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("Failed to start camera: {e}");
                    return;
                }
            };

            let width = capturer.width();
            let height = capturer.height();

            // VP8 encoder at camera resolution, 500 kbps
            let mut encoder = match Vp8Encoder::new(width, height, 500) {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!("Failed to create VP8 encoder: {e}");
                    return;
                }
            };

            tracing::info!(width, height, "Video capture pipeline started");

            loop {
                if stop_rx.try_recv().is_ok() {
                    tracing::info!("Video capture stopping");
                    break;
                }

                if let Some(frame) = capturer.try_recv() {
                    // Camera provides RGB→RGBA, convert to I420 for VP8 encoding
                    let i420 = rgba_to_i420(frame.width, frame.height, &frame.data);

                    match encoder.encode(&i420) {
                        Ok(packets) => {
                            for packet in packets {
                                let _ = io_cmd_tx.try_send(IoCommand::WriteVideo(packet.data));
                            }
                        }
                        Err(e) => {
                            tracing::debug!("VP8 encode error: {e}");
                        }
                    }
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        });

        Ok(())
    }

    /// Start the screen share capture pipeline: screen frames → VP8 → peer connection.
    pub fn start_screen_capture(
        &mut self,
        frame_rx: std::sync::mpsc::Receiver<VideoFrame>,
        io_cmd_tx: mpsc::Sender<IoCommand>,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        if self.capture_active {
            return Ok(());
        }

        let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        self.stop_tx = Some(stop_tx);
        self.capture_active = true;

        std::thread::spawn(move || {
            let mut encoder = match Vp8Encoder::new(width, height, 1500) {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!("Failed to create VP8 encoder for screen: {e}");
                    return;
                }
            };

            tracing::info!(width, height, "Screen capture pipeline started");

            loop {
                if stop_rx.try_recv().is_ok() {
                    tracing::info!("Screen capture stopping");
                    break;
                }

                match frame_rx.try_recv() {
                    Ok(frame) => {
                        // Screen capture frames are BGRA (from xcap)
                        let i420 =
                            elementium_codec::bgra_to_i420(frame.width, frame.height, &frame.data);

                        match encoder.encode(&i420) {
                            Ok(packets) => {
                                for packet in packets {
                                    let _ =
                                        io_cmd_tx.try_send(IoCommand::WriteVideo(packet.data));
                                }
                            }
                            Err(e) => {
                                tracing::debug!("VP8 encode error: {e}");
                            }
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        tracing::info!("Screen capture frame channel closed");
                        break;
                    }
                }
            }
        });

        Ok(())
    }

    /// Start the playback (decode) pipeline: peer connection → VP8 decode → frame buffer.
    pub fn start_playback(
        &mut self,
        event_rx: Arc<Mutex<mpsc::Receiver<PcEvent>>>,
        frame_buffer: VideoFrameBuffer,
        pc_id: String,
    ) -> Result<(), String> {
        if self.playback_active {
            return Ok(());
        }
        self.playback_active = true;

        std::thread::spawn(move || {
            let mut decoder = match Vp8Decoder::new() {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!("Failed to create VP8 decoder: {e}");
                    return;
                }
            };

            tracing::info!(pc_id = %pc_id, "Video playback pipeline started");

            loop {
                let event = {
                    let mut rx = match event_rx.lock() {
                        Ok(rx) => rx,
                        Err(_) => return,
                    };
                    rx.try_recv().ok()
                };

                match event {
                    Some(PcEvent::VideoData(vp8_packet)) => {
                        match decoder.decode(&vp8_packet) {
                            Ok(frames) => {
                                for i420_frame in frames {
                                    // Convert I420 to RGBA for display
                                    let rgba_frame =
                                        elementium_codec::i420_to_rgba(&i420_frame);

                                    // Store in the shared frame buffer
                                    let track_key =
                                        format!("{pc_id}-video");
                                    if let Ok(mut buf) = frame_buffer.lock() {
                                        buf.insert(track_key, rgba_frame);
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::debug!("VP8 decode error: {e}");
                            }
                        }
                    }
                    Some(_) => {
                        // Other events (audio, state changes) are not handled here
                    }
                    None => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                }
            }
        });

        Ok(())
    }

    /// Stop the capture pipeline.
    pub fn stop_capture(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.try_send(());
        }
        self.capture_active = false;
    }

    pub fn is_capture_active(&self) -> bool {
        self.capture_active
    }

    pub fn is_playback_active(&self) -> bool {
        self.playback_active
    }
}

impl Drop for VideoPipeline {
    fn drop(&mut self) {
        self.stop_capture();
    }
}

/// Convert RGBA pixel data to I420.
/// Camera provides RGB converted to RGBA (R, G, B, A byte order).
fn rgba_to_i420(width: u32, height: u32, rgba: &[u8]) -> elementium_types::I420Frame {
    let w = width as usize;
    let h = height as usize;

    let y_size = w * h;
    let uv_size = (w / 2) * (h / 2);

    let mut y_plane = vec![0u8; y_size];
    let mut u_plane = vec![0u8; uv_size];
    let mut v_plane = vec![0u8; uv_size];

    for row in 0..h {
        for col in 0..w {
            let px = (row * w + col) * 4;
            let r = rgba[px] as f32;
            let g = rgba[px + 1] as f32;
            let b = rgba[px + 2] as f32;

            // BT.601 conversion
            let y = (0.299 * r + 0.587 * g + 0.114 * b) as u8;
            y_plane[row * w + col] = y;

            if row % 2 == 0 && col % 2 == 0 {
                let u =
                    ((-0.169 * r - 0.331 * g + 0.500 * b) + 128.0).clamp(0.0, 255.0) as u8;
                let v =
                    ((0.500 * r - 0.419 * g - 0.081 * b) + 128.0).clamp(0.0, 255.0) as u8;

                let uv_idx = (row / 2) * (w / 2) + (col / 2);
                u_plane[uv_idx] = u;
                v_plane[uv_idx] = v;
            }
        }
    }

    elementium_types::I420Frame {
        width,
        height,
        y: y_plane,
        u: u_plane,
        v: v_plane,
        timestamp_us: 0,
    }
}
