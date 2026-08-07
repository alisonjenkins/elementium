use std::sync::atomic::{AtomicU32, Ordering};

/// A smoothed estimate of outbound packet loss, shared between the transport and encoder.
///
/// The Opus encoder needs to know how much loss to protect against: in-band FEC only
/// emits redundancy in proportion to `OPUS_SET_PACKET_LOSS_PERC`, so that number decides
/// how much of the bitrate is spent on resilience. It was previously a fixed guess of 10%,
/// which is wrong in both directions -- wasting bitrate on a clean link, and under-
/// protecting a bad one.
///
/// The real number is already available: RTCP receiver reports carry the fraction of our
/// packets each peer failed to receive, which str0m surfaces as `MediaEgressStats::loss`.
/// This type is the handoff between the thread that observes those reports and the
/// capture thread that owns the encoder.
///
/// ## Why smoothing is not optional
///
/// Per-report loss is noisy: a single report interval that happens to straddle a burst can
/// read 30% on a link that is really at 2%. Reconfiguring the encoder straight from each
/// report would make bitrate allocation oscillate audibly. Observations are folded in as
/// an exponentially weighted moving average, and callers are expected to act only on the
/// integer percentage, which changes far less often than the raw input.
#[derive(Debug)]
pub struct NetworkLossEstimate {
    /// Smoothed loss in hundredths of a percent (0..=10000), so the EWMA keeps useful
    /// resolution below 1% while staying in a lock-free atomic.
    centi_perc: AtomicU32,
}

/// Weight given to each new observation, as a percentage.
///
/// Low enough that one anomalous report cannot swing the estimate far, high enough that a
/// genuine change in link quality is reflected within a few report intervals (str0m emits
/// these once per second).
const OBSERVATION_WEIGHT_PERC: u32 = 25;

/// Largest loss percentage worth reporting to the encoder.
///
/// Above roughly this level, spending yet more bitrate on redundancy stops helping --
/// libopus itself caps the useful range, and the call has bigger problems than FEC tuning.
const MAX_LOSS_PERC: u32 = 40;

const CENTI: u32 = 100;

impl NetworkLossEstimate {
    /// Create an estimate seeded with a starting loss percentage.
    #[must_use]
    pub fn new(initial_perc: u8) -> Self {
        Self {
            centi_perc: AtomicU32::new(
                u32::from(initial_perc)
                    .min(MAX_LOSS_PERC)
                    .saturating_mul(CENTI),
            ),
        }
    }

    /// Fold in one RTCP-derived loss fraction (`0.0..=1.0`).
    ///
    /// Values outside that range, and non-finite ones, are ignored rather than clamped:
    /// they indicate a report we do not understand, and inventing a number from it would
    /// be worse than keeping the previous estimate.
    pub fn observe(&self, fraction_lost: f32) {
        if !fraction_lost.is_finite() || !(0.0..=1.0).contains(&fraction_lost) {
            return;
        }
        // 0.0..=1.0 scaled to hundredths of a percent, so 1.0 -> 10000. The guard above
        // has already rejected anything outside 0.0..=1.0 and every non-finite value, so
        // this rounds to an integer in 0..=10000 -- comfortably inside `u32`, with no
        // truncation or sign loss possible.
        #[allow(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let observed = (f64::from(fraction_lost) * 10_000.0).round() as u32;
        let observed = observed.min(MAX_LOSS_PERC.saturating_mul(CENTI));

        // Relaxed is sufficient: this is a hint, and the only consumer re-reads it on a
        // timer. `fetch_update` still gives a consistent read-modify-write, so concurrent
        // observations from several peer connections cannot lose each other entirely.
        let _ = self
            .centi_perc
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                let weighted_new = observed.saturating_mul(OBSERVATION_WEIGHT_PERC);
                let weighted_old =
                    current.saturating_mul(100_u32.saturating_sub(OBSERVATION_WEIGHT_PERC));
                Some(weighted_new.saturating_add(weighted_old) / 100)
            });
    }

    /// The current estimate, rounded to whole percent, for `OPUS_SET_PACKET_LOSS_PERC`.
    #[must_use]
    pub fn percent(&self) -> u8 {
        let centi = self.centi_perc.load(Ordering::Relaxed);
        let perc = centi.saturating_add(CENTI / 2) / CENTI;
        u8::try_from(perc.min(MAX_LOSS_PERC)).unwrap_or(u8::MAX)
    }
}

impl Default for NetworkLossEstimate {
    /// Start from zero loss and let the first reports move it.
    ///
    /// Deliberately not the old fixed 10% guess: on a clean link that guess permanently
    /// spent a chunk of the bitrate on redundancy that was never needed.
    fn default() -> Self {
        Self::new(0)
    }
}

#[cfg(test)]
mod network_loss_estimate_tests {
    use super::*;

    #[test]
    fn starts_at_the_seeded_value() {
        assert_eq!(NetworkLossEstimate::new(10).percent(), 10);
        assert_eq!(NetworkLossEstimate::default().percent(), 0);
    }

    /// A single bad report must not swing the estimate to that report's value -- that is
    /// exactly the oscillation the smoothing exists to prevent.
    #[test]
    fn one_outlier_report_moves_the_estimate_only_partway() {
        let estimate = NetworkLossEstimate::new(0);
        estimate.observe(0.40);
        let after_one = estimate.percent();
        assert!(
            after_one > 0 && after_one < 40,
            "expected a partial move, got {after_one}"
        );
    }

    /// Sustained loss must converge on the true value, or FEC would chronically
    /// under-protect a genuinely bad link.
    #[test]
    fn sustained_loss_converges_on_the_observed_rate() {
        let estimate = NetworkLossEstimate::new(0);
        for _ in 0..40 {
            estimate.observe(0.12);
        }
        let settled = estimate.percent();
        assert!(
            (11..=13).contains(&settled),
            "expected convergence near 12%, got {settled}"
        );
    }

    /// A link that recovers must let the estimate fall back, so bitrate stops being spent
    /// on redundancy that is no longer needed.
    #[test]
    fn recovery_lets_the_estimate_fall_again() {
        let estimate = NetworkLossEstimate::new(0);
        for _ in 0..40 {
            estimate.observe(0.30);
        }
        let bad = estimate.percent();
        for _ in 0..40 {
            estimate.observe(0.0);
        }
        let good = estimate.percent();
        assert!(
            bad > 20,
            "should have risen under sustained loss, got {bad}"
        );
        assert_eq!(good, 0, "should return to zero on a clean link, got {good}");
    }

    /// Nonsense reports must leave the estimate untouched rather than being clamped into
    /// a number we would then act on.
    #[test]
    fn malformed_observations_are_ignored() {
        let estimate = NetworkLossEstimate::new(7);
        estimate.observe(f32::NAN);
        estimate.observe(-0.5);
        estimate.observe(2.0);
        estimate.observe(f32::INFINITY);
        assert_eq!(estimate.percent(), 7);
    }

    /// Total loss is capped: past a point, more redundancy stops buying anything and the
    /// encoder should not be told to spend everything on it.
    #[test]
    fn extreme_loss_is_capped() {
        let estimate = NetworkLossEstimate::new(0);
        for _ in 0..100 {
            estimate.observe(1.0);
        }
        assert_eq!(estimate.percent(), 40);
    }
}
