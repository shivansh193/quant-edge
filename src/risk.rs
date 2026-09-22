//! Portfolio-level risk: exposure measurement and position-level controls.
//!
//! Two kinds of thing live here, and the distinction matters:
//!   * **Measurement** (`beta`, `var`, `cvar`, `sector_exposure`,
//!     `herfindahl_index`) — pure functions over returns/weights you already
//!     have. Reporting only; nothing here trades on its own.
//!   * **Controls** (`apply_position_cap`, `inverse_vol_weights`,
//!     `DrawdownBreaker`) — actually change what gets held. These are wired
//!     into the backtester as opt-in `BacktestConfig` fields, not applied by
//!     default, so existing behaviour doesn't change unless you ask for it.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Measurement ────────────────────────────────────────────────────────────────

/// OLS beta of `returns` against `benchmark` (same-length, paired by index).
/// `None` if there are fewer than 2 points or the benchmark has no variance.
pub fn beta(returns: &[f64], benchmark: &[f64]) -> Option<f64> {
    let n = returns.len().min(benchmark.len());
    if n < 2 {
        return None;
    }
    let (r, b) = (&returns[..n], &benchmark[..n]);
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let (mr, mb) = (mean(r), mean(b));
    let cov: f64 = r.iter().zip(b).map(|(x, y)| (x - mr) * (y - mb)).sum();
    let var_b: f64 = b.iter().map(|y| (y - mb).powi(2)).sum();
    (var_b > 1e-18).then_some(cov / var_b)
}

/// Historical Value-at-Risk: the loss such that `confidence` (e.g. 0.95) of
/// returns are better than it. Returned as a negative number (a loss), or
/// `None` for empty input. Uses linear interpolation between order statistics.
pub fn var(returns: &[f64], confidence: f64) -> Option<f64> {
    if returns.is_empty() {
        return None;
    }
    let mut sorted: Vec<f64> = returns.iter().copied().filter(|r| r.is_finite()).collect();
    if sorted.is_empty() {
        return None;
    }
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some(percentile(&sorted, (1.0 - confidence).clamp(0.0, 1.0) * 100.0))
}

/// Historical Conditional VaR (Expected Shortfall): the average of the worst
/// `1 - confidence` fraction of returns. Always at least as bad as `var`.
pub fn cvar(returns: &[f64], confidence: f64) -> Option<f64> {
    if returns.is_empty() {
        return None;
    }
    let mut sorted: Vec<f64> = returns.iter().copied().filter(|r| r.is_finite()).collect();
    if sorted.is_empty() {
        return None;
    }
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let tail_frac = (1.0 - confidence).clamp(1.0 / sorted.len() as f64, 1.0);
    let n = ((sorted.len() as f64) * tail_frac).ceil() as usize;
    let n = n.max(1).min(sorted.len());
    Some(sorted[..n].iter().sum::<f64>() / n as f64)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.len() == 1 {
        return sorted[0];
    }
    let rank = (p / 100.0) * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        sorted[lo]
    } else {
        let frac = rank - lo as f64;
        sorted[lo] * (1.0 - frac) + sorted[hi] * frac
    }
}

/// Portfolio weight grouped by an arbitrary key (sector, industry, ...).
/// Tickers missing from `ticker_group` are bucketed under `"Unknown"` — never
/// silently dropped, since an unmeasured exposure is exactly what a risk
/// report must not hide.
pub fn group_exposure(
    weights: &HashMap<String, f64>,
    ticker_group: &HashMap<String, String>,
) -> HashMap<String, f64> {
    let mut out: HashMap<String, f64> = HashMap::new();
    for (ticker, &w) in weights {
        let group = ticker_group.get(ticker).cloned().unwrap_or_else(|| "Unknown".to_string());
        *out.entry(group).or_insert(0.0) += w;
    }
    out
}

/// Herfindahl-Hirschman Index of portfolio weights: sum of squared weights,
/// in [1/n, 1]. Equal-weighting n names gives 1/n (the least concentrated
/// possible); a single name gives 1 (maximally concentrated).
pub fn herfindahl_index(weights: &HashMap<String, f64>) -> f64 {
    weights.values().map(|w| w * w).sum()
}

/// Effective number of positions implied by the HHI (1/HHI). For equal
/// weights this equals the actual count; concentrated books have a smaller
/// effective count than their nominal position count.
pub fn effective_positions(weights: &HashMap<String, f64>) -> f64 {
    let hhi = herfindahl_index(weights);
    if hhi > 1e-12 { 1.0 / hhi } else { 0.0 }
}

// ── Controls ──────────────────────────────────────────────────────────────────

/// Cap every position at `max_weight` of the portfolio, redistributing the
/// excess proportionally across uncapped positions, iterating until stable
/// (a cap can itself push another position over the limit). Weights that
/// don't sum to 1.0 are rescaled first so the result always sums to ~1.0
/// (unless `max_weight * n < 1.0`, which is infeasible and left over-cap
/// rather than silently changing position count).
pub fn apply_position_cap(weights: &HashMap<String, f64>, max_weight: f64) -> HashMap<String, f64> {
    if weights.is_empty() || max_weight <= 0.0 {
        return HashMap::new();
    }
    let total: f64 = weights.values().sum();
    if total <= 0.0 {
        return weights.clone();
    }
    let mut w: HashMap<String, f64> = weights.iter().map(|(k, &v)| (k.clone(), v / total)).collect();

    for _ in 0..w.len().max(1) {
        let excess: f64 = w.values().map(|&v| (v - max_weight).max(0.0)).sum();
        if excess < 1e-12 {
            break;
        }
        for v in w.values_mut() {
            if *v > max_weight {
                *v = max_weight;
            }
        }
        let uncapped_total: f64 = w.values().filter(|&&v| v < max_weight - 1e-12).sum();
        if uncapped_total < 1e-12 {
            break; // every position is at/over the cap: nothing left to redistribute into
        }
        for v in w.values_mut() {
            if *v < max_weight - 1e-12 {
                *v += excess * (*v / uncapped_total);
            }
        }
    }
    w
}

/// Inverse-volatility weights: `weight_i = (1/vol_i) / sum(1/vol_j)`. A name
/// with zero/invalid volatility gets zero weight rather than a division blow-up.
pub fn inverse_vol_weights(vols: &HashMap<String, f64>) -> HashMap<String, f64> {
    let inv: HashMap<String, f64> = vols
        .iter()
        .filter(|(_, &v)| v.is_finite() && v > 1e-12)
        .map(|(k, &v)| (k.clone(), 1.0 / v))
        .collect();
    let total: f64 = inv.values().sum();
    if total <= 0.0 {
        return vols.keys().map(|k| (k.clone(), 0.0)).collect();
    }
    let mut out: HashMap<String, f64> = inv.iter().map(|(k, &v)| (k.clone(), v / total)).collect();
    for k in vols.keys() {
        out.entry(k.clone()).or_insert(0.0);
    }
    out
}

/// A drawdown circuit breaker: once peak-to-trough loss exceeds `threshold`,
/// force to cash until it recovers to `resume_at` (a smaller drawdown than
/// `threshold`, so it doesn't flip back in immediately on noise).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrawdownBreaker {
    /// Force to cash once drawdown from the running peak exceeds this (e.g. 0.20 = 20%).
    pub threshold: f64,
    /// Resume normal exposure once drawdown recovers to at or below this.
    pub resume_at: f64,
}

impl Default for DrawdownBreaker {
    fn default() -> Self {
        Self { threshold: 0.20, resume_at: 0.10 }
    }
}

/// Runtime state of a `DrawdownBreaker` across a sequence of equity values.
#[derive(Debug, Clone, Copy, Default)]
pub struct BreakerState {
    peak: f64,
    tripped: bool,
}

impl DrawdownBreaker {
    /// Feed the next equity value; returns true if the breaker is (now, or
    /// still) tripped and the portfolio should be flat. `state` carries the
    /// running peak and trip status between calls — one per backtest.
    pub fn step(&self, state: &mut BreakerState, equity: f64) -> bool {
        if state.peak <= 0.0 || equity > state.peak {
            state.peak = equity;
        }
        let drawdown = if state.peak > 0.0 { (state.peak - equity) / state.peak } else { 0.0 };

        if !state.tripped && drawdown >= self.threshold {
            state.tripped = true;
        } else if state.tripped && drawdown <= self.resume_at {
            state.tripped = false;
        }
        state.tripped
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    // ── beta ──────────────────────────────────────────────────────────────────

    #[test]
    fn beta_of_a_series_against_itself_is_one() {
        let r = [0.01, -0.02, 0.03, 0.005, -0.01];
        assert!((beta(&r, &r).unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn beta_scales_linearly() {
        let b = [0.01, -0.02, 0.03, 0.005, -0.01];
        let r: Vec<f64> = b.iter().map(|x| x * 1.5).collect();
        assert!((beta(&r, &b).unwrap() - 1.5).abs() < 1e-9);
    }

    #[test]
    fn beta_of_an_inverse_series_is_negative_one() {
        let b = [0.01, -0.02, 0.03, 0.005, -0.01];
        let r: Vec<f64> = b.iter().map(|x| -x).collect();
        assert!((beta(&r, &b).unwrap() + 1.0).abs() < 1e-9);
    }

    #[test]
    fn beta_is_none_without_enough_data_or_benchmark_variance() {
        assert!(beta(&[0.01], &[0.02]).is_none());
        assert!(beta(&[0.01, 0.02, 0.03], &[0.05, 0.05, 0.05]).is_none());
    }

    // ── VaR / CVaR ────────────────────────────────────────────────────────────

    #[test]
    fn var_matches_hand_computed_linear_interpolation() {
        // 10 sorted values 1..10, p=30 -> rank = 0.3*9 = 2.7 (0-indexed),
        // interpolating 70/30 between sorted[2]=3 and sorted[3]=4.
        let r: Vec<f64> = (1..=10).map(|i| i as f64).collect();
        let v = var(&r, 0.70).unwrap();
        assert!((v - 3.7).abs() < 1e-9, "{v}");
    }

    #[test]
    fn cvar_averages_the_worst_tail_fraction() {
        // 100 returns, -0.01 through -0.10 and 90 zeros: the worst 10% by
        // count are exactly the ten negative values.
        let mut r: Vec<f64> = vec![0.0; 90];
        r.extend((1..=10).map(|i| -0.01 * i as f64));
        let cv = cvar(&r, 0.90).unwrap();
        let expected_cvar = -(1..=10).map(|i| 0.01 * i as f64).sum::<f64>() / 10.0;
        assert!((cv - expected_cvar).abs() < 1e-9, "{cv} vs {expected_cvar}");

        let v = var(&r, 0.90).unwrap();
        assert!(cv <= v, "CVaR must be at least as bad as VaR ({cv} vs {v})");
    }

    #[test]
    fn cvar_never_looks_at_fewer_than_one_observation() {
        // 3 points, 99% confidence: the tail fraction would round to 0
        // without a floor - must still return the single worst observation.
        let r = [0.05, -0.01, -0.20];
        assert!((cvar(&r, 0.99).unwrap() - (-0.20)).abs() < 1e-9);
    }

    #[test]
    fn var_cvar_empty_and_nan_handling() {
        assert!(var(&[], 0.95).is_none());
        assert!(cvar(&[], 0.95).is_none());
        let with_nan = [0.01, f64::NAN, -0.02];
        assert!(var(&with_nan, 0.5).is_some());
    }

    // ── exposure / concentration ──────────────────────────────────────────────

    #[test]
    fn group_exposure_sums_weights_per_group_and_buckets_unmapped_as_unknown() {
        let w = map(&[("A", 0.3), ("B", 0.2), ("C", 0.5)]);
        let g: HashMap<String, String> = [("A", "Tech"), ("B", "Tech")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let exp = group_exposure(&w, &g);
        assert!((exp["Tech"] - 0.5).abs() < 1e-9);
        assert!((exp["Unknown"] - 0.5).abs() < 1e-9, "C has no sector mapping");
    }

    #[test]
    fn herfindahl_and_effective_positions_bracket_correctly() {
        let equal10 = map(&(0..10).map(|i| (Box::leak(i.to_string().into_boxed_str()) as &str, 0.1)).collect::<Vec<_>>());
        assert!((herfindahl_index(&equal10) - 0.1).abs() < 1e-9);
        assert!((effective_positions(&equal10) - 10.0).abs() < 1e-6);

        let concentrated = map(&[("A", 0.97), ("B", 0.01), ("C", 0.01), ("D", 0.01)]);
        assert!(herfindahl_index(&concentrated) > 0.9);
        assert!(effective_positions(&concentrated) < 1.2);
    }

    // ── position cap ──────────────────────────────────────────────────────────

    #[test]
    fn position_cap_redistributes_excess_and_preserves_total() {
        // 3 positions: a 0.30 cap would make 1.0 unreachable (0.30*3=0.9), so
        // use a feasible cap (0.40*3=1.2) to test normal redistribution.
        let w = map(&[("A", 0.60), ("B", 0.20), ("C", 0.20)]);
        let capped = apply_position_cap(&w, 0.40);
        assert!(capped["A"] <= 0.40 + 1e-9);
        let total: f64 = capped.values().sum();
        assert!((total - 1.0).abs() < 1e-6, "total {total}");
        // B and C should have grown to absorb A's excess, and stay equal
        // (they started equal).
        assert!((capped["B"] - capped["C"]).abs() < 1e-9);
        assert!(capped["B"] > 0.20);
    }

    #[test]
    fn position_cap_handles_a_cascade_where_redistribution_itself_breaches_the_cap() {
        // A is way over, B and C are just under the cap: naive one-pass
        // redistribution would push B/C over 0.34 too.
        let w = map(&[("A", 0.80), ("B", 0.11), ("C", 0.09)]);
        let capped = apply_position_cap(&w, 0.34);
        for (t, v) in &capped {
            assert!(*v <= 0.34 + 1e-6, "{t} = {v} exceeds the cap");
        }
        let total: f64 = capped.values().sum();
        assert!((total - 1.0).abs() < 1e-6);
    }

    #[test]
    fn position_cap_renormalises_weights_that_do_not_sum_to_one() {
        let w = map(&[("A", 60.0), ("B", 20.0), ("C", 20.0)]); // sums to 100
        let capped = apply_position_cap(&w, 0.40); // feasible: 0.40*3 >= 1.0
        let total: f64 = capped.values().sum();
        assert!((total - 1.0).abs() < 1e-6, "total {total}");
    }

    #[test]
    fn position_cap_edge_cases() {
        assert!(apply_position_cap(&HashMap::new(), 0.3).is_empty());
        assert!(apply_position_cap(&map(&[("A", 1.0)]), 0.0).is_empty());
        // A cap so tight it's infeasible (0.1 * 3 < 1.0): every position ends
        // up at the cap rather than looping forever.
        let w = map(&[("A", 0.4), ("B", 0.3), ("C", 0.3)]);
        let capped = apply_position_cap(&w, 0.1);
        for v in capped.values() {
            assert!((*v - 0.1).abs() < 1e-6);
        }
    }

    // ── inverse-vol sizing ────────────────────────────────────────────────────

    #[test]
    fn inverse_vol_weights_favour_the_calmer_name() {
        let v = map(&[("CALM", 0.10), ("WILD", 0.40)]);
        let w = inverse_vol_weights(&v);
        assert!(w["CALM"] > w["WILD"]);
        assert!((w["CALM"] + w["WILD"] - 1.0).abs() < 1e-9);
        // Exactly proportional to 1/vol: CALM should be 4x WILD (0.40/0.10).
        assert!((w["CALM"] / w["WILD"] - 4.0).abs() < 1e-6);
    }

    #[test]
    fn inverse_vol_weights_zero_out_invalid_entries_without_crashing() {
        let v = map(&[("OK", 0.10), ("ZERO", 0.0), ("NEG", -1.0)]);
        let w = inverse_vol_weights(&v);
        assert_eq!(w["ZERO"], 0.0);
        assert_eq!(w["NEG"], 0.0);
        assert!((w["OK"] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn inverse_vol_weights_all_invalid_returns_zeros_not_nan() {
        let v = map(&[("A", 0.0), ("B", -1.0)]);
        let w = inverse_vol_weights(&v);
        assert!(w.values().all(|&x| x == 0.0));
    }

    // ── drawdown breaker ──────────────────────────────────────────────────────

    #[test]
    fn breaker_trips_on_threshold_and_releases_with_hysteresis() {
        let b = DrawdownBreaker { threshold: 0.20, resume_at: 0.10 };
        let mut s = BreakerState::default();
        assert!(!b.step(&mut s, 100.0)); // new peak
        assert!(!b.step(&mut s, 85.0)); // -15%, under threshold
        assert!(b.step(&mut s, 78.0)); // -22%, trips
        assert!(b.step(&mut s, 85.0)); // -15%: still tripped (above resume_at)
        assert!(!b.step(&mut s, 92.0)); // -8%: releases
    }

    #[test]
    fn breaker_does_not_flip_flop_on_noise_right_at_the_threshold() {
        // Hysteresis gap (threshold=0.20, resume_at=0.10) must prevent
        // trip/release/trip on every small wiggle around 20%.
        let b = DrawdownBreaker { threshold: 0.20, resume_at: 0.10 };
        let mut s = BreakerState::default();
        b.step(&mut s, 100.0);
        assert!(b.step(&mut s, 79.5)); // trips at -20.5%
        assert!(b.step(&mut s, 81.0)); // -19%: still tripped, not released
        assert!(b.step(&mut s, 79.0)); // -21%: still tripped
    }

    #[test]
    fn breaker_tracks_a_rising_peak() {
        let b = DrawdownBreaker::default();
        let mut s = BreakerState::default();
        b.step(&mut s, 100.0);
        b.step(&mut s, 120.0);
        assert!((s.peak - 120.0).abs() < 1e-9);
        // A 15% drop from the NEW peak (120 -> 102) must not trip a 20% breaker.
        assert!(!b.step(&mut s, 102.0));
    }
}
