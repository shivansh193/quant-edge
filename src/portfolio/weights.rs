use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use crate::roles::classifier::Role;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WeightMode {
    Equal,
    /// Role-level multipliers — e.g. FastestGrower gets 2x, DeepValue 0.5x.
    /// Normalised to sum to 1.0 before use.
    RoleWeighted(HashMap<Role, f64>),
}

/// ticker → portfolio weight (0.0–1.0, sums to 1.0)
pub type WeightMap = HashMap<String, f64>;

/// Build a WeightMap from active role assignments.
///
/// `assignments` is a flat list of (ticker, role) pairs across all industries.
/// One ticker can hold multiple roles — their weights are summed before normalisation.
pub fn compute_weights(
    assignments: &[(String, Role)],
    mode: &WeightMode,
) -> WeightMap {
    if assignments.is_empty() {
        return HashMap::new();
    }

    // Accumulate raw weights per ticker
    let mut raw: HashMap<String, f64> = HashMap::new();

    for (ticker, role) in assignments {
        let role_weight = match mode {
            WeightMode::Equal => 1.0,
            WeightMode::RoleWeighted(multipliers) => {
                *multipliers.get(role).unwrap_or(&1.0)
            }
        };
        *raw.entry(ticker.clone()).or_insert(0.0) += role_weight;
    }

    // Normalise so weights sum to 1.0
    let total: f64 = raw.values().sum();
    raw.into_iter()
        .map(|(ticker, w)| (ticker, w / total))
        .collect()
}

/// Equal-weight convenience constructor.
pub fn equal_weights(tickers: &[String]) -> WeightMap {
    if tickers.is_empty() {
        return HashMap::new();
    }
    let w = 1.0 / tickers.len() as f64;
    tickers.iter().map(|t| (t.clone(), w)).collect()
}