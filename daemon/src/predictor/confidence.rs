//! Confidence scoring for predictions.
//!
//! A prediction is only useful if the daemon knows when to keep quiet. The score
//! blends three independent signals, then multiplies by how much the event source
//! can be trusted (a `/proc` scan is worth less than a `wm_on_resume` event).

#[derive(Debug, Clone, Copy)]
pub struct ConfidenceInput {
    pub samples: u64,
    pub min_samples: u64,
    pub top1: f64,
    pub top2: f64,
    pub entropy: f64,
    pub candidates: usize,
    pub source_trust: f64,
}

/// Returns 0.0..=1.0.
pub fn score(i: &ConfidenceInput) -> f64 {
    if i.samples == 0 || i.top1 <= 0.0 {
        return 0.0;
    }
    let coverage = (i.samples as f64 / i.min_samples.max(1) as f64).min(1.0);
    let margin = ((i.top1 - i.top2) / i.top1.max(1e-6)).clamp(0.0, 1.0);
    let max_entropy = (i.candidates.max(2) as f64).log2();
    let certainty = (1.0 - (i.entropy / max_entropy).clamp(0.0, 1.0)).clamp(0.0, 1.0);

    let raw = 0.45 * coverage + 0.35 * margin + 0.20 * certainty;
    (raw * i.source_trust.clamp(0.0, 1.0)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> ConfidenceInput {
        ConfidenceInput {
            samples: 100,
            min_samples: 40,
            top1: 0.6,
            top2: 0.2,
            entropy: 1.2,
            candidates: 3,
            source_trust: 1.0,
        }
    }

    #[test]
    fn strong_signal_scores_high() {
        assert!(score(&base()) > 0.6);
    }

    #[test]
    fn no_samples_scores_zero() {
        let mut i = base();
        i.samples = 0;
        assert_eq!(score(&i), 0.0);
    }

    #[test]
    fn low_trust_source_cannot_reach_high_confidence() {
        let mut i = base();
        i.source_trust = 0.5;
        assert!(score(&i) <= 0.5);
    }

    #[test]
    fn flat_distribution_scores_low() {
        let mut i = base();
        i.top1 = 0.34;
        i.top2 = 0.33;
        i.entropy = 1.58;
        assert!(score(&i) < 0.5);
    }

    #[test]
    fn under_sampled_is_penalised() {
        let mut i = base();
        i.samples = 5;
        assert!(score(&i) < score(&base()));
    }
}
