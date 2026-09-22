pub mod morning;
pub mod evening;
pub mod backfill;

pub use morning::{build_auto_universe, run_morning};
pub use evening::run_evening;

use crate::signals::SignalScore;

/// Score-based dynamic top-N selection.
/// Returns all picks with score ≥ threshold, clamped to [5, 30].
/// If fewer than 5 clear the threshold, fall back to top-5 by score.
pub fn score_based_picks(scores: &[SignalScore], threshold: f64) -> Vec<SignalScore> {
    // Risk-off: hold cash. Without this the "fall back to top-5" rule below
    // would keep recommending longs precisely when the macro gate says not to.
    if scores.iter().any(|s| !s.macro_on) {
        return Vec::new();
    }

    let filtered: Vec<SignalScore> = scores
        .iter()
        .filter(|s| s.composite >= threshold)
        .cloned()
        .collect();

    if filtered.len() < 5 {
        scores.iter().take(5).cloned().collect()
    } else if filtered.len() > 30 {
        filtered.into_iter().take(30).collect()
    } else {
        filtered
    }
}

/// Read MIN_SCORE_THRESHOLD from .env (default 60).
pub fn min_score_threshold() -> f64 {
    std::env::var("MIN_SCORE_THRESHOLD")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60.0)
}

/// Read HOLDING_PERIOD_DAYS from .env (default 25).
pub fn holding_period_days() -> u32 {
    std::env::var("HOLDING_PERIOD_DAYS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(25)
}
