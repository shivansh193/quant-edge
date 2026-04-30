use std::sync::Arc;

use crate::signals::{clamp_signal, mean, std_dev, MarketData, Signal};

use super::CorrelationData;

// ── Pairs mean-reversion signal ───────────────────────────────────────────────

/// Detects when a stock's GICS industry has diverged from its historically
/// correlated peers, scoring the expected mean reversion.
///
/// Score in [-1, +1]:
///   +1  industry significantly underperformed peers → expect reversion up
///   -1  industry significantly outperformed peers  → expect reversion down
///    0  no divergence or insufficient data
pub struct PairsSignal {
    data: Arc<CorrelationData>,
}

impl PairsSignal {
    pub fn new(data: Arc<CorrelationData>) -> Self {
        Self { data }
    }
}

impl Signal for PairsSignal {
    fn name(&self) -> &str {
        "Pairs"
    }

    fn compute(&self, _ticker: &str, market_data: &MarketData) -> f64 {
        let industry = &market_data.industry_name;

        // Find up to 3 industries correlated at r > 0.6
        let peers = self.data.top_correlated(industry, 0.6, 3);
        if peers.is_empty() {
            return 0.0;
        }

        let Some(ind_ret_map) = self.data.industry_returns.get(industry) else {
            return 0.0;
        };

        // Build chronological date list for this industry
        let mut dates: Vec<chrono::NaiveDate> = ind_ret_map.keys().copied().collect();
        dates.sort_unstable();

        // Need at least 20 dates for a meaningful std-dev, plus 10 for recent window
        if dates.len() < 20 {
            return 0.0;
        }

        // Build spread series: industry_return[d] - mean(peer_return[d])
        let mut spread_series: Vec<f64> = Vec::with_capacity(dates.len());

        for &date in &dates {
            let ind_r = ind_ret_map[&date];

            let peer_rets: Vec<f64> = peers
                .iter()
                .filter_map(|(peer_ind, _)| {
                    self.data
                        .industry_returns
                        .get(peer_ind)?
                        .get(&date)
                        .copied()
                })
                .filter(|v| v.is_finite())
                .collect();

            if peer_rets.is_empty() {
                continue;
            }

            let peer_avg = peer_rets.iter().sum::<f64>() / peer_rets.len() as f64;
            spread_series.push(ind_r - peer_avg);
        }

        if spread_series.len() < 20 {
            return 0.0;
        }

        let spread_std = std_dev(&spread_series);
        if spread_std < 1e-9 {
            return 0.0;
        }

        // Recent divergence: mean of last 10 spread values
        let recent = &spread_series[spread_series.len().saturating_sub(10)..];
        let recent_divergence = mean(recent);

        // Negate: positive divergence (outperformance) → negative signal (mean-reverts down)
        clamp_signal(-(recent_divergence / (1.5 * spread_std)))
    }
}
