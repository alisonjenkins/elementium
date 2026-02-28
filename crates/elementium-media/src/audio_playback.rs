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
    pub fn play(&self, frame: AudioFrame) {
        let _ = self.sender.try_send(frame);
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }
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
                        match rx.try_recv() {
                            Ok(frame) => {
                                sample_buf = frame.data;
                                buf_pos = 0;
                            }
                            Err(_) => {
                                // No data available — output silence
                                for sample in &mut output[written..] {
                                    *sample = 0.0;
                                }
                                return;
                            }
                        }
                    }

                    let available = sample_buf.len() - buf_pos;
                    let needed = output.len() - written;
                    let to_copy = available.min(needed);

                    output[written..written + to_copy]
                        .copy_from_slice(&sample_buf[buf_pos..buf_pos + to_copy]);
                    buf_pos += to_copy;
                    written += to_copy;
                }
            },
            err_fn,
            None,
        )
        .map_err(|e| PlaybackError::Stream(e.to_string()))?;

    Ok(stream)
}
