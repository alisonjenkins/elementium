use cpal::traits::{DeviceTrait, HostTrait};
use elementium_types::{MediaDevice, MediaDeviceKind};

/// Enumerate all available audio input and output devices.
pub fn enumerate_audio_devices() -> Vec<MediaDevice> {
    let host = cpal::default_host();
    let mut devices = Vec::new();

    // Input devices (microphones)
    if let Ok(inputs) = host.input_devices() {
        for (i, device) in inputs.enumerate() {
            let label = device
                .name()
                .unwrap_or_else(|_| format!("Microphone {i}"));
            devices.push(MediaDevice {
                id: format!("audio-input-{i}"),
                label,
                kind: MediaDeviceKind::AudioInput,
            });
        }
    }

    // Output devices (speakers)
    if let Ok(outputs) = host.output_devices() {
        for (i, device) in outputs.enumerate() {
            let label = device.name().unwrap_or_else(|_| format!("Speaker {i}"));
            devices.push(MediaDevice {
                id: format!("audio-output-{i}"),
                label,
                kind: MediaDeviceKind::AudioOutput,
            });
        }
    }

    devices
}

/// Get the default input device config (sample rate, channels).
pub fn default_input_config() -> Option<(u32, u16)> {
    let host = cpal::default_host();
    let device = host.default_input_device()?;
    let config = device.default_input_config().ok()?;
    Some((config.sample_rate().0, config.channels()))
}

/// Get the default output device config.
pub fn default_output_config() -> Option<(u32, u16)> {
    let host = cpal::default_host();
    let device = host.default_output_device()?;
    let config = device.default_output_config().ok()?;
    Some((config.sample_rate().0, config.channels()))
}
