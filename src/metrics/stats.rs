use chrono::NaiveDate;
use rand::prelude::*;
use rand::rngs::StdRng;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::portfolio::engine::{PortfolioSnapshot, SimulationResult};

// ── Output types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrawdownPeriod {
    pub peak_date:     NaiveDate,
    pub trough_date:   NaiveDate,
    pub recovery_date: Option<NaiveDate>, // None if still in drawdown at end
    pub drawdown_pct:  f64,               // e.g. -0.32 = 32% drawdown
    pub recovery_days: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonteCarloResult {
    pub n_simulations:    usize,
    pub median_return:    f64,
    pub percentile_5:     f64,    // worst 5% of random portfolios
    pub percentile_95:    f64,    // best 5%
    pub beat_strategy_pct: f64,   // % of random runs that beat our strategy
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsReport {
    // Return metrics
    pub total_return:       f64,    // e.g. 0.45 = 45%
    pub annualised_return:  f64,
    pub benchmark_return:   f64,
    pub alpha:              f64,    // total_return - benchmark_return
    pub annualised_alpha:   f64,

    // Risk metrics
    pub sharpe_ratio:       f64,    // annualised, daily excess returns over risk_free_annual
    pub sortino_ratio:      f64,    // downside deviation only
    pub max_drawdown:       f64,    // worst peak-to-trough
    pub volatility:         f64,    // annualised daily return std dev
    pub calmar_ratio:       f64,    // annualised_return / |max_drawdown|

    // Drawdown detail
    pub drawdown_periods:   Vec<DrawdownPeriod>,

    // vs random baseline
    pub monte_carlo:        Option<MonteCarloResult>,

    // Metadata
    pub trading_days:       usize,
    pub total_swaps:        usize,
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MetricsOptions {
    pub run_monte_carlo:  bool,
    /// Annual risk-free rate for Sharpe/Sortino (0.0 = raw return / risk).
    pub risk_free_annual: f64,
    pub mc_simulations:   usize,
    pub mc_seed:          u64,
}

impl Default for MetricsOptions {
    fn default() -> Self {
        Self { run_monte_carlo: false, risk_free_annual: 0.0, mc_simulations: 5_000, mc_seed: 42 }
    }
}

pub fn compute_metrics(result: &SimulationResult, run_monte_carlo: bool) -> MetricsReport {
    compute_metrics_with(result, &MetricsOptions { run_monte_carlo, ..Default::default() })
}

pub fn compute_metrics_with(result: &SimulationResult, opts: &MetricsOptions) -> MetricsReport {
    let snapshots  = &result.snapshots;
    let total_days = snapshots.len();

    if total_days < 2 {
        return MetricsReport::empty();
    }

    let initial = result.config.initial_capital;
    let final_v = result.final_value();
    let bench_v = result.benchmark_final();

    // ── Daily returns ─────────────────────────────────────────────────────────

    let portfolio_returns = daily_returns_from_snapshots(snapshots, false);

    // ── Annualisation factor ──────────────────────────────────────────────────

    let years = total_days as f64 / 252.0;

    // ── Return metrics ────────────────────────────────────────────────────────

    let total_return      = (final_v - initial) / initial;
    let benchmark_return  = (bench_v - initial) / initial;
    let alpha             = total_return - benchmark_return;

    let annualised_return = (1.0 + total_return).powf(1.0 / years) - 1.0;
    let bench_annualised  = (1.0 + benchmark_return).powf(1.0 / years) - 1.0;
    let annualised_alpha  = annualised_return - bench_annualised;

    // ── Risk metrics ──────────────────────────────────────────────────────────

    let rf_daily = (1.0 + opts.risk_free_annual).powf(1.0 / 252.0) - 1.0;
    let volatility = std_dev(&portfolio_returns) * 252_f64.sqrt();
    let sharpe_ratio = sharpe(&portfolio_returns, rf_daily);
    let sortino_ratio = sortino(&portfolio_returns, rf_daily);

    let (max_drawdown, drawdown_periods) = compute_drawdowns(snapshots);
    let calmar_ratio = if max_drawdown.abs() > 0.0 {
        annualised_return / max_drawdown.abs()
    } else {
        0.0
    };

    // ── Monte Carlo baseline ──────────────────────────────────────────────────

    let monte_carlo = if opts.run_monte_carlo {
        Some(monte_carlo_baseline(result, opts.mc_simulations, opts.mc_seed))
    } else {
        None
    };

    info!(
        "Metrics: total_return={:.2}%  sharpe={:.2}  max_dd={:.2}%  alpha={:.2}%",
        total_return * 100.0,
        sharpe_ratio,
        max_drawdown * 100.0,
        alpha * 100.0,
    );

    MetricsReport {
        total_return,
        annualised_return,
        benchmark_return,
        alpha,
        annualised_alpha,
        sharpe_ratio,
        sortino_ratio,
        max_drawdown,
        volatility,
        calmar_ratio,
        drawdown_periods,
        monte_carlo,
        trading_days: total_days,
        total_swaps:  result.swap_log.len(),
    }
}

/// Annualised Sharpe: mean daily excess return / its std dev, × √252.
fn sharpe(returns: &[f64], rf_daily: f64) -> f64 {
    let excess: Vec<f64> = returns.iter().map(|r| r - rf_daily).collect();
    let sd = std_dev(&excess);
    if sd < 1e-12 { 0.0 } else { mean(&excess) / sd * 252_f64.sqrt() }
}

/// Annualised Sortino: mean daily excess return / downside deviation, × √252.
fn sortino(returns: &[f64], rf_daily: f64) -> f64 {
    let dd = downside_deviation(returns, rf_daily);
    if dd < 1e-12 { 0.0 } else { (mean(returns) - rf_daily) / dd * 252_f64.sqrt() }
}

// ── Monte Carlo ───────────────────────────────────────────────────────────────

/// Compare the strategy against `n` random, equal-weight, buy-and-hold
/// portfolios drawn **without replacement from the whole universe**, each the
/// same size as the strategy's portfolio.
///
/// (An earlier version drew `n` names from a pool of exactly `n` names — the
/// strategy's own holdings — so every "random" portfolio was identical and the
/// beat-rate was meaningless.)
///
/// `n_simulations == 0` in the result means it could not be computed (the
/// universe is no larger than the portfolio).
pub fn monte_carlo_baseline(
    result: &SimulationResult,
    n_simulations: usize,
    seed: u64,
) -> MonteCarloResult {
    let not_computed = |why: &str| {
        warn!("Monte Carlo baseline skipped: {why}");
        MonteCarloResult {
            n_simulations: 0,
            median_return: 0.0,
            percentile_5: 0.0,
            percentile_95: 0.0,
            beat_strategy_pct: 0.0,
        }
    };

    // Typical portfolio size = median number of live holdings.
    let mut sizes: Vec<usize> = result
        .snapshots
        .iter()
        .map(|s| s.holdings.len())
        .filter(|&n| n > 0)
        .collect();
    if sizes.is_empty() {
        return not_computed("strategy held nothing");
    }
    sizes.sort_unstable();
    let k = sizes[sizes.len() / 2];

    // Sorted so a fixed seed gives a fixed answer (HashMap order is random).
    let mut pool: Vec<(&String, f64)> = result
        .universe_returns
        .iter()
        .filter(|(_, r)| r.is_finite())
        .map(|(t, &r)| (t, r))
        .collect();
    pool.sort_by(|a, b| a.0.cmp(b.0));

    if pool.len() <= k {
        return not_computed(&format!(
            "universe of {} names is not larger than the {}-name portfolio",
            pool.len(),
            k
        ));
    }

    let strategy_return = result.total_return();
    let mut rng = StdRng::seed_from_u64(seed);
    let mut sims: Vec<f64> = (0..n_simulations)
        .map(|_| {
            let chosen: Vec<&(&String, f64)> = pool.choose_multiple(&mut rng, k).collect();
            chosen.iter().map(|(_, r)| *r).sum::<f64>() / k as f64
        })
        .collect();
    sims.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let beat = sims.iter().filter(|&&r| r > strategy_return).count();
    let out = MonteCarloResult {
        n_simulations,
        median_return: percentile(&sims, 50.0),
        percentile_5: percentile(&sims, 5.0),
        percentile_95: percentile(&sims, 95.0),
        beat_strategy_pct: beat as f64 / n_simulations.max(1) as f64 * 100.0,
    };

    info!(
        "Monte Carlo ({} sims of {} of {} names): median={:.2}%  p5={:.2}%  p95={:.2}%  random beat strategy {:.1}%",
        n_simulations, k, pool.len(),
        out.median_return * 100.0, out.percentile_5 * 100.0, out.percentile_95 * 100.0,
        out.beat_strategy_pct,
    );
    out
}

// ── Drawdown computation ──────────────────────────────────────────────────────

fn compute_drawdowns(
    snapshots: &[PortfolioSnapshot],
) -> (f64, Vec<DrawdownPeriod>) {
    if snapshots.is_empty() {
        return (0.0, Vec::new());
    }

    let mut peak_value  = snapshots[0].portfolio_value;
    let mut peak_date   = snapshots[0].date;
    let mut in_drawdown = false;
    let _trough_value = peak_value;
    let _trough_date  = peak_date;

    let mut max_drawdown = 0.0_f64;
    let mut periods: Vec<DrawdownPeriod> = Vec::new();

    // Track the current open drawdown period
    let mut open_peak_date   = peak_date;
    let mut open_trough_date = peak_date;
    let mut open_drawdown    = 0.0_f64;

    for snap in snapshots {
        let v = snap.portfolio_value;

        if v >= peak_value {
            // New high — if we were in drawdown, close it
            if in_drawdown {
                let recovery_days = (snap.date - open_trough_date).num_days() as u64;
                periods.push(DrawdownPeriod {
                    peak_date:     open_peak_date,
                    trough_date:   open_trough_date,
                    recovery_date: Some(snap.date),
                    drawdown_pct:  open_drawdown,
                    recovery_days: Some(recovery_days),
                });
                in_drawdown = false;
            }
            peak_value = v;
            peak_date  = snap.date;
        } else {
            let dd = (v - peak_value) / peak_value; // negative number

            if !in_drawdown {
                // Entering a new drawdown
                in_drawdown      = true;
                open_peak_date   = peak_date;
                open_trough_date = snap.date;
                open_drawdown    = dd;
            } else if dd < open_drawdown {
                // Drawdown is worsening
                open_trough_date = snap.date;
                open_drawdown    = dd;
            }

            if dd < max_drawdown {
                max_drawdown = dd;
            }
        }
    }

    // Close any open drawdown at end of simulation
    if in_drawdown {
        periods.push(DrawdownPeriod {
            peak_date:     open_peak_date,
            trough_date:   open_trough_date,
            recovery_date: None,
            drawdown_pct:  open_drawdown,
            recovery_days: None,
        });
    }

    // Sort by severity
    periods.sort_by(|a, b| {
        a.drawdown_pct.partial_cmp(&b.drawdown_pct).unwrap_or(std::cmp::Ordering::Equal)
    });

    (max_drawdown, periods)
}

// ── Statistical helpers ───────────────────────────────────────────────────────

fn daily_returns_from_snapshots(
    snapshots: &[PortfolioSnapshot],
    use_benchmark: bool,
) -> Vec<f64> {
    snapshots
        .windows(2)
        .map(|w| {
            let prev = if use_benchmark { w[0].benchmark_value } else { w[0].portfolio_value };
            let curr = if use_benchmark { w[1].benchmark_value } else { w[1].portfolio_value };
            if prev > 0.0 { (curr - prev) / prev } else { 0.0 }
        })
        .collect()
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() { return 0.0; }
    values.iter().sum::<f64>() / values.len() as f64
}

fn std_dev(values: &[f64]) -> f64 {
    if values.len() < 2 { return 0.0; }
    let m = mean(values);
    let variance = values.iter().map(|v| (v - m).powi(2)).sum::<f64>()
        / (values.len() - 1) as f64;
    variance.sqrt()
}

/// Downside deviation: √(mean of min(0, r − threshold)² over **all** returns).
fn downside_deviation(returns: &[f64], threshold: f64) -> f64 {
    if returns.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = returns
        .iter()
        .map(|&r| (r - threshold).min(0.0).powi(2))
        .sum();
    (sum_sq / returns.len() as f64).sqrt()
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() { return 0.0; }
    let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// ── Empty report ──────────────────────────────────────────────────────────────

impl MetricsReport {
    fn empty() -> Self {
        MetricsReport {
            total_return:      0.0,
            annualised_return: 0.0,
            benchmark_return:  0.0,
            alpha:             0.0,
            annualised_alpha:  0.0,
            sharpe_ratio:      0.0,
            sortino_ratio:     0.0,
            max_drawdown:      0.0,
            volatility:        0.0,
            calmar_ratio:      0.0,
            drawdown_periods:  Vec::new(),
            monte_carlo:       None,
            trading_days:      0,
            total_swaps:       0,
        }
    }
}
// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portfolio::engine::{RebalanceFrequency, SimulationConfig};
    use crate::portfolio::weights::WeightMode;
    use chrono::Duration;
    use std::collections::HashMap;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn config() -> SimulationConfig {
        SimulationConfig {
            start_date: d("2024-01-01"),
            end_date: d("2024-12-31"),
            initial_capital: 1_000.0,
            rebalance_freq: RebalanceFrequency::Monthly,
            weight_mode: WeightMode::Equal,
            active_roles: vec![],
            benchmark_ticker: "^GSPC".into(),
            transaction_cost_bps: 0.0,
        }
    }

    /// Snapshots growing at `daily` per day vs. a benchmark growing at `bench`.
    fn result(daily: f64, bench: f64, n: usize, universe_returns: HashMap<String, f64>) -> SimulationResult {
        let mut snaps = Vec::new();
        for i in 0..n {
            let pv = 1_000.0 * (1.0 + daily).powi(i as i32);
            let mut holdings = HashMap::new();
            for t in ["H1", "H2", "H3"] {
                holdings.insert(t.to_string(), pv / 3.0);
            }
            snaps.push(PortfolioSnapshot {
                date: d("2024-01-01") + Duration::days(i as i64),
                portfolio_value: pv,
                benchmark_value: 1_000.0 * (1.0 + bench).powi(i as i32),
                holdings,
            });
        }
        SimulationResult {
            snapshots: snaps,
            swap_log: vec![],
            role_performance: vec![],
            industry_perf: vec![],
            config: config(),
            universe_returns,
        }
    }

    fn universe_of(n: usize) -> HashMap<String, f64> {
        (0..n).map(|i| (format!("T{i:02}"), -0.5 + i as f64 * 0.05)).collect()
    }

    #[test]
    fn monte_carlo_draws_differ_so_the_distribution_is_not_degenerate() {
        // Regression: the old code sampled n-of-n, giving p5 == p95 == median.
        let r = result(0.001, 0.0, 30, universe_of(30));
        let mc = monte_carlo_baseline(&r, 2_000, 7);
        assert_eq!(mc.n_simulations, 2_000);
        assert!(mc.percentile_95 > mc.percentile_5 + 0.05, "p5={} p95={}", mc.percentile_5, mc.percentile_95);
    }

    #[test]
    fn monte_carlo_beat_rate_reflects_relative_performance() {
        // Strategy ≈ +300%: nothing random can match it.
        let strong = result(0.05, 0.0, 30, universe_of(30));
        assert!(monte_carlo_baseline(&strong, 1_000, 1).beat_strategy_pct < 1.0);
        // Strategy ≈ −60%: nearly every random portfolio beats it.
        let weak = result(-0.03, 0.0, 30, universe_of(30));
        assert!(monte_carlo_baseline(&weak, 1_000, 1).beat_strategy_pct > 99.0);
    }

    #[test]
    fn monte_carlo_is_reproducible_for_a_fixed_seed() {
        let r = result(0.001, 0.0, 30, universe_of(30));
        let a = monte_carlo_baseline(&r, 500, 99);
        let b = monte_carlo_baseline(&r, 500, 99);
        assert_eq!(a.median_return, b.median_return);
        assert_eq!(a.beat_strategy_pct, b.beat_strategy_pct);
    }

    #[test]
    fn monte_carlo_is_skipped_when_the_universe_is_no_bigger_than_the_portfolio() {
        let r = result(0.001, 0.0, 30, universe_of(3)); // 3 holdings, 3 names
        assert_eq!(monte_carlo_baseline(&r, 500, 1).n_simulations, 0);
        let empty = result(0.001, 0.0, 30, HashMap::new());
        assert_eq!(monte_carlo_baseline(&empty, 500, 1).n_simulations, 0);
    }

    #[test]
    fn risk_free_rate_lowers_sharpe_and_sortino() {
        // Noisy but positive.
        let mut r = result(0.0, 0.0, 60, universe_of(30));
        for (i, s) in r.snapshots.iter_mut().enumerate() {
            let noise = if i % 2 == 0 { 0.004 } else { -0.001 };
            s.portfolio_value = 1_000.0 * (1.0 + 0.0015 * i as f64 + noise);
        }
        let raw = compute_metrics_with(&r, &MetricsOptions::default());
        let with_rf = compute_metrics_with(&r, &MetricsOptions { risk_free_annual: 0.25, ..Default::default() });
        assert!(raw.sharpe_ratio > with_rf.sharpe_ratio);
        assert!(raw.sortino_ratio > with_rf.sortino_ratio);
    }

    #[test]
    fn alpha_is_strategy_minus_benchmark() {
        let r = result(0.002, 0.0005, 100, universe_of(30));
        let m = compute_metrics(&r, false);
        assert!(m.alpha > 0.0);
        assert!((m.alpha - (m.total_return - m.benchmark_return)).abs() < 1e-12);
        assert!(m.annualised_alpha > 0.0);
    }

    #[test]
    fn steady_growth_has_no_drawdown() {
        let m = compute_metrics(&result(0.001, 0.0, 50, universe_of(30)), false);
        assert_eq!(m.max_drawdown, 0.0);
        assert!(m.drawdown_periods.is_empty());
    }

    #[test]
    fn drawdown_is_detected_with_its_recovery() {
        let mut r = result(0.0, 0.0, 6, universe_of(30));
        for (s, v) in r.snapshots.iter_mut().zip([100.0, 120.0, 60.0, 90.0, 121.0, 125.0]) {
            s.portfolio_value = v;
        }
        r.config.initial_capital = 100.0;
        let (max_dd, periods) = compute_drawdowns(&r.snapshots);
        assert!((max_dd + 0.5).abs() < 1e-9, "max dd {max_dd}");
        assert_eq!(periods.len(), 1);
        assert!(periods[0].recovery_date.is_some());
    }

    #[test]
    fn downside_deviation_counts_all_observations() {
        // One −2% day among four flat days: √(0.02² / 5) = 0.00894…
        let dd = downside_deviation(&[0.0, 0.0, -0.02, 0.0, 0.0], 0.0);
        assert!((dd - (0.02f64.powi(2) / 5.0).sqrt()).abs() < 1e-12);
        assert_eq!(downside_deviation(&[0.01, 0.02], 0.0), 0.0);
        assert_eq!(downside_deviation(&[], 0.0), 0.0);
    }

    #[test]
    fn too_few_snapshots_yields_an_empty_report() {
        let r = result(0.001, 0.0, 1, universe_of(30));
        assert_eq!(compute_metrics(&r, true).trading_days, 0);
    }

    #[test]
    fn percentile_picks_the_expected_element() {
        let v = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(percentile(&v, 0.0), 1.0);
        assert_eq!(percentile(&v, 50.0), 3.0);
        assert_eq!(percentile(&v, 100.0), 5.0);
        assert_eq!(percentile(&[], 50.0), 0.0);
    }
}
