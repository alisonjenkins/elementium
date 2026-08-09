//! Automatic gain control for the microphone.
//!
//! Every caller in this stack asks for it -- `getUserMedia` is invoked with
//! `autoGainControl: true` -- and until now the request was accepted and ignored. A browser
//! lifts a quiet microphone; we transmitted it exactly as quiet as it arrived, and Opus
//! encoded near-silence, which the far end hears as "I cannot hear you, you are breaking
//! up".
//!
//! Deliberately conservative, because the failure modes of an aggressive AGC are worse than
//! the problem it solves:
//!
//! - It follows a *decaying peak envelope*, not the level of one 20ms buffer, so the gain
//!   does not lurch between syllables.
//! - It rises slowly and falls quickly. Coming up too fast pumps room noise into every
//!   pause; coming down slowly would let a sudden loud passage clip.
//! - It holds its gain during silence rather than winding up. An AGC that keeps amplifying
//!   an empty room arrives at a wall of hiss the moment anyone breathes.
//! - It never clips. The gain applied to a buffer is capped at whatever keeps that buffer's
//!   own peak under full scale, computed before anything is multiplied, so the output
//!   cannot exceed the ceiling however far behind the envelope has fallen.
//!
//! What it does not do is compress: the dynamic range within a phrase is left alone. This
//! makes a quiet microphone audible, and does not try to make a good one louder than it is.

/// Peak level the gain aims to put the envelope at, about -9 dBFS.
///
/// Loud enough to be comfortably above where Opus starts treating input as silence, with
/// headroom left for a sudden louder syllable.
const TARGET_PEAK: f32 = 0.35;

/// Never amplify by more than this, about +30 dB.
///
/// A cap matters more than the number: without one, a muted or unplugged input drives the
/// gain to whatever the arithmetic allows, and the first real sound arrives distorted.
const MAX_GAIN: f32 = 32.0;

/// Never attenuate below this, about -12 dB.
const MIN_GAIN: f32 = 0.25;

/// Below this envelope level the input is a quiet room, not a person, and the gain is held
/// where it is. About -60 dBFS.
const NOISE_FLOOR: f32 = 0.001;

/// How much of the envelope survives each 20ms buffer: roughly a 1.3 second memory.
const ENVELOPE_DECAY: f32 = 0.985;

/// Share of the distance to the target taken per buffer while getting louder.
///
/// About a second to cross most of the gap, which is slow enough not to be heard happening.
const RISE_RATE: f32 = 0.03;

/// Share taken per buffer while getting quieter, roughly ten times faster than the rise.
const FALL_RATE: f32 = 0.3;

/// The highest sample the output may contain, leaving a little room below full scale.
const CEILING: f32 = 0.98;

/// Tracks the microphone's level and applies a slowly-moving gain to it.
#[derive(Debug)]
pub struct AutoGain {
    gain: f32,
    envelope: f32,
}

impl Default for AutoGain {
    fn default() -> Self {
        Self::new()
    }
}

impl AutoGain {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            gain: 1.0,
            envelope: 0.0,
        }
    }

    /// The gain currently being applied, for reporting.
    #[must_use]
    pub const fn gain(&self) -> f32 {
        self.gain
    }

    /// The level the microphone is being judged at, for reporting.
    #[must_use]
    pub const fn envelope(&self) -> f32 {
        self.envelope
    }

    /// Apply gain to one buffer of mono samples, in place.
    pub fn apply(&mut self, samples: &mut [f32]) {
        let buffer_peak = samples.iter().fold(0.0_f32, |max, s| max.max(s.abs()));
        self.envelope = buffer_peak.max(self.envelope * ENVELOPE_DECAY);

        // Gated on what just arrived, not on the envelope.
        //
        // Gating on the envelope looks equivalent and is not: after speech stops, the
        // envelope decays for seconds before it falls under the floor, and every buffer of
        // that decay asks for more gain. A test of an eight-second silence found the gain
        // at the cap by the end of it -- the empty-room wind-up this is supposed to
        // prevent, written into the thing preventing it.
        if buffer_peak > NOISE_FLOOR {
            let desired = (TARGET_PEAK / self.envelope).clamp(MIN_GAIN, MAX_GAIN);
            let rate = if desired < self.gain {
                FALL_RATE
            } else {
                RISE_RATE
            };
            self.gain = (desired - self.gain).mul_add(rate, self.gain);
        }

        // Capped against this buffer's own peak, so the output cannot clip no matter how
        // far the envelope has drifted from what just arrived.
        let safe_gain = if buffer_peak > 0.0 {
            self.gain.min(CEILING / buffer_peak)
        } else {
            self.gain
        };
        if (safe_gain - 1.0).abs() < f32::EPSILON {
            return;
        }
        for sample in samples.iter_mut() {
            *sample *= safe_gain;
        }
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::{AutoGain, CEILING, MAX_GAIN, TARGET_PEAK};

    /// One 20ms buffer at 48kHz of a constant amplitude, alternating sign so the peak is
    /// the amplitude and the signal is not DC.
    fn buffer(amplitude: f32) -> Vec<f32> {
        (0..960)
            .map(|i| if i % 2 == 0 { amplitude } else { -amplitude })
            .collect()
    }

    /// Run `count` buffers of the same level through, returning the last output.
    fn settle(agc: &mut AutoGain, amplitude: f32, count: usize) -> Vec<f32> {
        let mut out = Vec::new();
        for _ in 0..count {
            out = buffer(amplitude);
            agc.apply(&mut out);
        }
        out
    }

    /// The point of the whole module: a quiet microphone ends up audible.
    #[test]
    fn a_quiet_microphone_is_brought_up() {
        let mut agc = AutoGain::new();
        // -34 dBFS, about what a real call measured before this existed.
        let out = settle(&mut agc, 0.02, 400);
        let peak = out.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
        assert!(
            peak > 0.2,
            "a quiet input should end up near the target, got a peak of {peak}"
        );
        assert!(peak <= CEILING, "and must not exceed the ceiling: {peak}");
    }

    /// An input that is already at a sensible level is left roughly alone.
    #[test]
    fn a_healthy_microphone_is_not_pushed_around() {
        let mut agc = AutoGain::new();
        let out = settle(&mut agc, TARGET_PEAK, 400);
        let peak = out.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
        assert!(
            (peak - TARGET_PEAK).abs() < 0.1,
            "expected to stay near {TARGET_PEAK}, got {peak}"
        );
    }

    /// Nothing may clip, however suddenly the input gets louder.
    ///
    /// This is the failure that would make the AGC worse than no AGC: gain wound up for a
    /// whisper, then someone laughs.
    #[test]
    fn a_sudden_loud_passage_does_not_clip() {
        let mut agc = AutoGain::new();
        settle(&mut agc, 0.01, 400);
        let mut loud = buffer(0.9);
        agc.apply(&mut loud);
        let peak = loud.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
        assert!(peak <= CEILING, "output clipped at {peak}");
    }

    /// Silence must not wind the gain up, or the next word arrives as a bang.
    #[test]
    fn silence_does_not_wind_the_gain_up() {
        let mut agc = AutoGain::new();
        settle(&mut agc, TARGET_PEAK, 200);
        let settled = agc.gain();
        settle(&mut agc, 0.0, 500);
        assert!(
            (agc.gain() - settled).abs() < 0.01,
            "gain moved from {settled} to {} across silence",
            agc.gain()
        );
    }

    /// The gain has to move slowly enough not to be heard doing it.
    #[test]
    fn the_gain_does_not_lurch_between_buffers() {
        let mut agc = AutoGain::new();
        settle(&mut agc, 0.5, 50);
        let before = agc.gain();
        let mut quiet = buffer(0.01);
        agc.apply(&mut quiet);
        // A single 20ms buffer may not change the gain by more than a few percent.
        assert!(
            (agc.gain() / before) < 1.1,
            "gain jumped from {before} to {} in one buffer",
            agc.gain()
        );
    }

    /// A dead input must not drive the gain to the cap and stay there.
    #[test]
    fn the_gain_is_capped() {
        let mut agc = AutoGain::new();
        // Above the noise floor, so the gain really is driven at the cap rather than
        // simply never adapting.
        settle(&mut agc, 0.002, 2000);
        assert!(agc.gain() <= MAX_GAIN, "gain exceeded its cap: {}", agc.gain());
    }
}
