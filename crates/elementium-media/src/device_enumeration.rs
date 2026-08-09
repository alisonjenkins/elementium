use cpal::traits::{DeviceTrait, HostTrait};
use elementium_types::{MediaDevice, MediaDeviceKind};

/// Enumerate all available audio input and output devices.
#[must_use]
pub fn enumerate_audio_devices() -> Vec<MediaDevice> {
    let host = cpal::default_host();
    let mut devices = Vec::new();

    // Input devices (microphones)
    match host.input_devices() {
        Ok(inputs) => {
            for (i, device) in inputs.enumerate() {
                let label = device.name().unwrap_or_else(|_| format!("Microphone {i}"));
                devices.push(MediaDevice {
                    id: format!("audio-input-{i}"),
                    label,
                    kind: MediaDeviceKind::AudioInput,
                });
            }
        }
        Err(e) => {
            // Distinct from "no microphones are plugged in": a cpal failure here (the host
            // API itself refusing to enumerate) removed the entire input device class from
            // the list, and looked in the UI exactly like a machine with no microphones --
            // there was nothing to tell the two apart. Logged so the difference survives.
            tracing::error!(error = %e, "failed to enumerate audio input devices");
        }
    }

    // Output devices (speakers)
    match host.output_devices() {
        Ok(outputs) => {
            for (i, device) in outputs.enumerate() {
                let label = device.name().unwrap_or_else(|_| format!("Speaker {i}"));
                devices.push(MediaDevice {
                    id: format!("audio-output-{i}"),
                    label,
                    kind: MediaDeviceKind::AudioOutput,
                });
            }
        }
        Err(e) => {
            // Same reasoning as the input-device case above.
            tracing::error!(error = %e, "failed to enumerate audio output devices");
        }
    }

    devices
}

/// Get the default input device config (sample rate, channels).
#[must_use]
pub fn default_input_config() -> Option<(u32, u16)> {
    let host = cpal::default_host();
    let device = host.default_input_device()?;
    let config = device.default_input_config().ok()?;
    Some((config.sample_rate().0, config.channels()))
}

/// Get the default output device config.
#[must_use]
pub fn default_output_config() -> Option<(u32, u16)> {
    let host = cpal::default_host();
    let device = host.default_output_device()?;
    let config = device.default_output_config().ok()?;
    Some((config.sample_rate().0, config.channels()))
}
