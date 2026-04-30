use std::collections::HashMap;
use std::sync::Arc;

use crate::signals::SignalScore;

use super::CorrelationData;

// ── Warning type ──────────────────────────────────────────────────────────────

/// Attached to a lower-ranked pick when its GICS industry is highly correlated
/// (r > 0.7) with a higher-ranked pick's industry.
#[derive(Debug, Clone)]
pub struct CorrelationWarning {
    pub correlated_with: String, // ticker of the higher-ranked pick
    pub correlation: f64,
}

// ── Guard ─────────────────────────────────────────────────────────────────────

pub struct ConcentrationGuard;

impl ConcentrationGuard {
    /// Inspect the top 10 ranked picks and flag any pair whose industries have
    /// r > 0.7. Only the lower-ranked ticker (higher rank number) is flagged.
    /// Returns a map of ticker → first warning found for that ticker.
    pub fn check(
        scores: &[SignalScore],
        corr_data: &Arc<CorrelationData>,
    ) -> HashMap<String, CorrelationWarning> {
        const HIGH_CORR: f64 = 0.7;
        let top10: Vec<&SignalScore> = scores.iter().take(10).collect();
        let mut warnings: HashMap<String, CorrelationWarning> = HashMap::new();

        for i in 0..top10.len() {
            for j in (i + 1)..top10.len() {
                let higher = top10[i]; // lower rank number = better score
                let lower = top10[j];

                if higher.industry == lower.industry {
                    // Same industry — always correlated; warn the lower-ranked
                    warnings.entry(lower.ticker.clone()).or_insert(CorrelationWarning {
                        correlated_with: higher.ticker.clone(),
                        correlation: 1.0,
                    });
                    continue;
                }

                let r = corr_data
                    .get_correlation(&higher.industry, &lower.industry)
                    .unwrap_or(0.0);

                if r > HIGH_CORR {
                    warnings.entry(lower.ticker.clone()).or_insert(CorrelationWarning {
                        correlated_with: higher.ticker.clone(),
                        correlation: r,
                    });
                }
            }
        }

        warnings
    }
}
