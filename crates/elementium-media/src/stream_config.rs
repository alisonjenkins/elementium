//! Picking a concrete audio stream config, rather than trusting cpal's "default".
//!
//! `default_output_config()` / `default_input_config()` are not queries of the device's
//! actual running rate. When the backend doesn't report a default, cpal synthesises one
//! with `cmp_default_heuristics`, which explicitly biases toward 44100Hz ("CD quality")
//! over any other supported rate.
//!
//! On Linux "default" almost always routes through `PipeWire`'s ALSA compatibility shim,
//! which runs its whole graph at one fixed rate (commonly 48000Hz) regardless of what an
//! individual ALSA client asks for -- ALSA's `set_rate(..., ValueOr::Nearest)` lets it
//! silently substitute a different rate than requested, and cpal never reads the
//! negotiated rate back afterwards. If the graph is really running at 48000Hz while every
//! layer above believes it's 44100Hz, that is a ~9% sample-rate error: on output it is
//! audibly indistinguishable from harsh digital noise, and on input it means we hand the
//! Opus encoder samples that are mislabelled by 9%.
//!
//! Preferring 48000Hz explicitly -- Opus's native rate, so this also removes a resample
//! step in the common case -- sidesteps that bias instead of trusting it blindly.

/// Opus's native sample rate, and the rate both capture and playback aim for.
pub const PREFERRED_RATE: u32 = 48_000;

/// Pick a [`cpal::SupportedStreamConfig`] running at exactly `preferred_rate` from a
/// device's advertised ranges, preferring stereo and `f32` when more than one range covers
/// that rate.
///
/// Returns `None` if no advertised range covers `preferred_rate` at all, in which case the
/// caller should fall back to the device's own default.
///
/// Takes an iterator rather than a device so the selection logic is testable with
/// synthetic ranges -- exercising it against a real device isn't possible in headless CI.
pub fn pick_preferred_config(
    configs: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    preferred_rate: u32,
) -> Option<cpal::SupportedStreamConfig> {
    configs
        .filter(|c| {
            c.min_sample_rate().0 <= preferred_rate && preferred_rate <= c.max_sample_rate().0
        })
        .max_by(|a, b| {
            let stereo = (a.channels() == 2).cmp(&(b.channels() == 2));
            let format = (a.sample_format() == cpal::SampleFormat::F32)
                .cmp(&(b.sample_format() == cpal::SampleFormat::F32));
            stereo.then(format)
        })
        .map(|c| c.with_sample_rate(cpal::SampleRate(preferred_rate)))
}
