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
    pub fn start() -> Result<Self, CaptureError> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or(CaptureError::NoDevice)?;

        let config = device
            .default_input_config()
            .map_err(|e| CaptureError::Config(e.to_string()))?;

        let sample_rate = config.sample_rate().0;
        let channels = config.channels();
        let (tx, rx) = mpsc::channel();

        let stream = match config.sample_format() {
            SampleFormat::F32 => build_stream::<f32>(&device, &config.into(), tx, sample_rate, channels),
            SampleFormat::I16 => build_stream::<i16>(&device, &config.into(), tx, sample_rate, channels),
            SampleFormat::U16 => build_stream::<u16>(&device, &config.into(), tx, sample_rate, channels),
            _ => return Err(CaptureError::Config("unsupported sample format".into())),
        }?;

        stream
            .play()
            .map_err(|e| CaptureError::Stream(e.to_string()))?;

        Ok(Self {
            _stream: stream,
            receiver: rx,
            sample_rate,
            channels,
        })
    }

    /// Receive the next audio frame (blocking).
    pub fn recv(&self) -> Option<AudioFrame> {
        self.receiver.recv().ok()
    }

    /// Try to receive an audio frame (non-blocking).
    pub fn try_recv(&self) -> Option<AudioFrame> {
        self.receiver.try_recv().ok()
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }
}

fn build_stream<T: cpal::Sample + cpal::SizedSample + Into<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    tx: mpsc::Sender<AudioFrame>,
    sample_rate: u32,
    channels: u16,
) -> Result<Stream, CaptureError> {
    let err_fn = |err| tracing::error!("Audio capture error: {err}");

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
