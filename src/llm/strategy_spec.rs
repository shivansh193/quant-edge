use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StrategySpec {
    pub name: String,
    pub signal_weights: SignalWeightOverride,
    pub filters: StrategyFilters,
    pub universe_override: Option<Vec<String>>,
    /// How many top picks to return (1–50, default 10)
    pub top_n: usize,
    /// Holding period in days (1–365, default 30)
    pub holding_period_days: u32,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct SignalWeightOverride {
    pub momentum:    Option<f64>,
    pub fundamental: Option<f64>,
    pub insider:     Option<f64>,
    pub sentiment:   Option<f64>,
    pub pairs:       Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StrategyFilters {
    pub min_market_cap_b:    Option<f64>,
    /// GICS sector names to include (null = all)
    pub sectors:             Option<Vec<String>>,
    /// GICS sector names to exclude
    pub exclude_sectors:     Option<Vec<String>>,
    pub macro_filter_enabled: bool,
    /// Drop picks below this composite score (0–100)
    pub min_score:           Option<f64>,
}

impl Default for StrategyFilters {
    fn default() -> Self {
        Self {
            min_market_cap_b:    None,
            sectors:             None,
            exclude_sectors:     None,
            macro_filter_enabled: true,
            min_score:           None,
        }
    }
}

impl Default for StrategySpec {
    fn default() -> Self {
        Self {
            name:             "Default".to_string(),
            signal_weights:   SignalWeightOverride::default(),
            filters:          StrategyFilters::default(),
            universe_override: None,
            top_n:            10,
            holding_period_days: 30,
        }
    }
}

impl StrategySpec {
    /// Validate and normalise the spec in place.
    /// Returns Err if any value is fundamentally invalid.
    pub fn validate(&mut self) -> Result<()> {
        if self.top_n == 0 {
            self.top_n = 1;
        }
        if self.top_n > 50 {
            bail!("top_n must be between 1 and 50, got {}", self.top_n);
        }

        if self.holding_period_days == 0 {
            self.holding_period_days = 1;
        }
        if self.holding_period_days > 365 {
            bail!(
                "holding_period_days must be between 1 and 365, got {}",
                self.holding_period_days
            );
        }

        // Normalise weights if their sum exceeds 1.0
        let w = &self.signal_weights;
        let total: f64 = [w.momentum, w.fundamental, w.insider, w.sentiment, w.pairs]
            .iter()
            .filter_map(|v| *v)
            .sum();

        if total > 1.0 + 1e-9 {
            let scale = 1.0 / total;
            let sw = &mut self.signal_weights;
            if let Some(v) = sw.momentum.as_mut()    { *v *= scale; }
            if let Some(v) = sw.fundamental.as_mut() { *v *= scale; }
            if let Some(v) = sw.insider.as_mut()     { *v *= scale; }
            if let Some(v) = sw.sentiment.as_mut()   { *v *= scale; }
            if let Some(v) = sw.pairs.as_mut()       { *v *= scale; }
        }

        Ok(())
    }
}
