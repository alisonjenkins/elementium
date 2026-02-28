use elementium_types::AudioFrame;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum OpusError {
    #[error("Opus encoder error: {0}")]
    Encoder(String),
    #[error("Opus decoder error: {0}")]
    Decoder(String),
}

/// Wraps `opus::Encoder` for encoding PCM audio to Opus.
pub struct OpusEncoder {
    inner: opus::Encoder,
    sample_rate: u32,
    channels: u16,
}

impl OpusEncoder {
    pub fn new(sample_rate: u32, channels: u16) -> Result<Self, OpusError> {
        let ch = match channels {
            1 => opus::Channels::Mono,
            2 => opus::Channels::Stereo,
            _ => return Err(OpusError::Encoder(format!("unsupported channel count: {channels}"))),
        };

        let encoder = opus::Encoder::new(sample_rate, ch, opus::Application::Voip)
            .map_err(|e| OpusError::Encoder(e.to_string()))?;

        Ok(Self {
            inner: encoder,
            sample_rate,
            channels,
        })
    }

    /// Encode a frame of f32 PCM samples to Opus. Returns encoded bytes.
    pub fn encode(&mut self, frame: &AudioFrame) -> Result<Vec<u8>, OpusError> {
        // Opus max packet size
        let mut output = vec![0u8; 4000];
        let len = self
            .inner
            .encode_float(&frame.data, &mut output)
            .map_err(|e| OpusError::Encoder(e.to_string()))?;
        output.truncate(len);
        Ok(output)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }
}

/// Wraps `opus::Decoder` for decoding Opus packets to PCM audio.
pub struct OpusDecoder {
    inner: opus::Decoder,
    sample_rate: u32,
    channels: u16,
}

impl OpusDecoder {
    pub fn new(sample_rate: u32, channels: u16) -> Result<Self, OpusError> {
        let ch = match channels {
            1 => opus::Channels::Mono,
            2 => opus::Channels::Stereo,
            _ => return Err(OpusError::Decoder(format!("unsupported channel count: {channels}"))),
        };

        let decoder = opus::Decoder::new(sample_rate, ch)
            .map_err(|e| OpusError::Decoder(e.to_string()))?;

        Ok(Self {
            inner: decoder,
            sample_rate,
            channels,
        })
    }

    /// Decode an Opus packet to f32 PCM samples. Returns an AudioFrame.
    /// `frame_size` is the number of samples per channel expected.
    pub fn decode(&mut self, packet: &[u8], frame_size: usize) -> Result<AudioFrame, OpusError> {
        let total_samples = frame_size * self.channels as usize;
        let mut output = vec![0.0f32; total_samples];
        let decoded = self
            .inner
            .decode_float(packet, &mut output, false)
            .map_err(|e| OpusError::Decoder(e.to_string()))?;
        output.truncate(decoded * self.channels as usize);

        Ok(AudioFrame {
            sample_rate: self.sample_rate,
            channels: self.channels,
            data: output,
            timestamp_us: 0,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_roundtrip() {
        let sample_rate = 48000;
        let channels = 1u16;
        let frame_size = 960; // 20ms at 48kHz

        let mut encoder = OpusEncoder::new(sample_rate, channels).unwrap();
        let mut decoder = OpusDecoder::new(sample_rate, channels).unwrap();

        // Generate a 440Hz sine wave
        let samples: Vec<f32> = (0..frame_size)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin())
            .collect();

        let frame = AudioFrame {
            sample_rate,
            channels,
            data: samples.clone(),
            timestamp_us: 0,
        };

        let encoded = encoder.encode(&frame).unwrap();
        assert!(!encoded.is_empty());
        assert!(encoded.len() < samples.len() * 4); // Should be compressed

        let decoded = decoder.decode(&encoded, frame_size).unwrap();
        assert_eq!(decoded.data.len(), frame_size);
        assert_eq!(decoded.sample_rate, sample_rate);
        assert_eq!(decoded.channels, channels);
    }
}
