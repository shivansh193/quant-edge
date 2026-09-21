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
///
/// For `RoleWeighted`: each of the 7 roles receives equal weight (1/7), and each
/// stock within a role receives equal weight (1/N where N = stocks in that role).
/// Optional per-role multipliers in the HashMap scale these base weights before
/// final normalisation.
pub fn compute_weights(
    assignments: &[(String, Role)],
    mode: &WeightMode,
) -> WeightMap {
    if assignments.is_empty() {
        return HashMap::new();
    }

    match mode {
        WeightMode::Equal => equal_weights(
            &assignments.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>(),
        ),
        WeightMode::RoleWeighted(multipliers) => {
            // Group tickers by role
            let mut role_buckets: HashMap<&Role, Vec<&String>> = HashMap::new();
            for (ticker, role) in assignments {
                role_buckets.entry(role).or_default().push(ticker);
            }

            let n_roles = role_buckets.len().max(1) as f64;
            let mut raw: HashMap<String, f64> = HashMap::new();

            for (role, tickers) in &role_buckets {
                let n_in_role = tickers.len().max(1) as f64;
                // Base: 1/n_roles per role, 1/n_in_role within role
                let base = (1.0 / n_roles) / n_in_role;
                // Apply optional multiplier (default 1.0)
                let mult = multipliers.get(*role).copied().unwrap_or(1.0);
                let w = base * mult;
                for ticker in tickers {
                    *raw.entry((*ticker).clone()).or_insert(0.0) += w;
                }
            }

            // Normalise so weights sum exactly to 1.0
            let total: f64 = raw.values().sum();
            if total < 1e-12 {
                return equal_weights(
                    &assignments.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>(),
                );
            }
            raw.into_iter().map(|(t, w)| (t, w / total)).collect()
        }
    }
}

/// Equal-weight convenience constructor.
pub fn equal_weights(tickers: &[String]) -> WeightMap {
    if tickers.is_empty() {
        return HashMap::new();
    }
    let w = 1.0 / tickers.len() as f64;
    tickers.iter().map(|t| (t.clone(), w)).collect()
}