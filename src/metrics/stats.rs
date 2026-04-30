use anyhow::Result;
use chrono::NaiveDate;
use rand::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::info;
use rand::prelude::*;
use rand::rngs::StdRng;

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
    pub sharpe_ratio:       f64,    // annualised, risk-free = 0
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

pub fn compute_metrics(result: &SimulationResult, run_monte_carlo: bool) -> MetricsReport {
    let snapshots  = &result.snapshots;
    let total_days = snapshots.len();

    if total_days < 2 {
        return MetricsReport::empty();
    }

    let initial = result.config.initial_capital;
    let final_v = result.final_value();
    let bench_v = result.benchmark_final();

    // ── Daily returns ─────────────────────────────────────────────────────────

    let portfolio_returns  = daily_returns_from_snapshots(snapshots, false);
    let benchmark_returns  = daily_returns_from_snapshots(snapshots, true);

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

    let volatility    = std_dev(&portfolio_returns) * 252_f64.sqrt();
    let sharpe_ratio  = if volatility > 0.0 { annualised_return / volatility } else { 0.0 };

    let downside_dev  = downside_deviation(&portfolio_returns, 0.0) * 252_f64.sqrt();
    let sortino_ratio = if downside_dev > 0.0 { annualised_return / downside_dev } else { 0.0 };

    let (max_drawdown, drawdown_periods) = compute_drawdowns(snapshots);
    let calmar_ratio = if max_drawdown.abs() > 0.0 {
        annualised_return / max_drawdown.abs()
    } else {
        0.0
    };

    // ── Monte Carlo baseline ──────────────────────────────────────────────────

    let monte_carlo = if run_monte_carlo {
        Some(monte_carlo_baseline(
            result,
            5_000,   // number of random portfolios
            42,      // rng seed for reproducibility
        ))
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

// ── Monte Carlo ───────────────────────────────────────────────────────────────

/// Build `n` random portfolios from the same universe of tickers,
/// same cap filter, same number of stocks — but random selection and
/// equal weight. Compare their final returns to our strategy.
pub fn monte_carlo_baseline(
    result: &SimulationResult,
    n_simulations: usize,
    seed: u64,
) -> MonteCarloResult {
    let snapshots = &result.snapshots;
    if snapshots.is_empty() {
        return MonteCarloResult {
            n_simulations,
            median_return: 0.0,
            percentile_5:  0.0,
            percentile_95: 0.0,
            beat_strategy_pct: 100.0,
        };
    }

    // Collect all tickers and their per-day price relative to day 0
    // We use the holdings from the first snapshot as our universe proxy
    let all_tickers: Vec<String> = snapshots[0].holdings.keys().cloned().collect();
    let n_stocks = all_tickers.len();

    if n_stocks == 0 {
        return MonteCarloResult {
            n_simulations,
            median_return: 0.0,
            percentile_5:  0.0,
            percentile_95: 0.0,
            beat_strategy_pct: 100.0,
        };
    }

    // Build ticker → [daily_value_relative] from snapshot holdings
    // relative = holding_value_day_t / holding_value_day_0
    let ticker_relatives: HashMap<String, Vec<f64>> = build_ticker_relatives(snapshots);

    let strategy_return = result.total_return();

    let mut rng = StdRng::seed_from_u64(seed);
    let mut sim_returns: Vec<f64> = Vec::with_capacity(n_simulations);

    for _ in 0..n_simulations {
        // Pick a random subset of same size as our strategy portfolio
        let chosen: Vec<&String> = all_tickers
            .choose_multiple(&mut rng, n_stocks)
            .collect();

        // Equal-weight portfolio return
        let portfolio_return = random_portfolio_return(&chosen, &ticker_relatives);
        sim_returns.push(portfolio_return);
    }

    sim_returns.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let median_return   = percentile(&sim_returns, 50.0);
    let percentile_5    = percentile(&sim_returns, 5.0);
    let percentile_95   = percentile(&sim_returns, 95.0);
    let beat_count      = sim_returns.iter().filter(|&&r| r > strategy_return).count();
    let beat_strategy_pct = beat_count as f64 / n_simulations as f64 * 100.0;

    info!(
        "Monte Carlo ({} sims): median={:.2}%  p5={:.2}%  p95={:.2}%  \
         random beat strategy {:.1}% of the time",
        n_simulations,
        median_return * 100.0,
        percentile_5 * 100.0,
        percentile_95 * 100.0,
        beat_strategy_pct,
    );

    MonteCarloResult {
        n_simulations,
        median_return,
        percentile_5,
        percentile_95,
        beat_strategy_pct,
    }
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
    let mut trough_value = peak_value;
    let mut trough_date  = peak_date;

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
    periods.sort_by(|a, b| a.drawdown_pct.partial_cmp(&b.drawdown_pct).unwrap());

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

/// Downside deviation — std dev of returns below `threshold` (typically 0).
fn downside_deviation(returns: &[f64], threshold: f64) -> f64 {
    let negatives: Vec<f64> = returns
        .iter()
        .filter(|&&r| r < threshold)
        .map(|&r| (r - threshold).powi(2))
        .collect();

    if negatives.is_empty() { return 0.0; }

    let mean_sq = negatives.iter().sum::<f64>() / negatives.len() as f64;
    mean_sq.sqrt()
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() { return 0.0; }
    let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// ── Monte Carlo helpers ───────────────────────────────────────────────────────

/// Build ticker → [value_relative_to_day0] from snapshot holdings.
/// Relative = value_on_day_t / value_on_day_0.
fn build_ticker_relatives(snapshots: &[PortfolioSnapshot]) -> HashMap<String, Vec<f64>> {
    let mut out: HashMap<String, Vec<f64>> = HashMap::new();

    // Seed initial values from day 0
    let day0 = &snapshots[0];
    for (ticker, &v0) in &day0.holdings {
        if v0 > 0.0 {
            out.insert(ticker.clone(), vec![1.0]);
        }
    }

    // Walk forward
    for snap in snapshots.iter().skip(1) {
        for (ticker, relatives) in &mut out {
            let v0 = day0.holdings.get(ticker).copied().unwrap_or(0.0);
            let vt = snap.holdings.get(ticker).copied().unwrap_or(0.0);
            let rel = if v0 > 0.0 { vt / v0 } else { 1.0 };
            relatives.push(rel);
        }
    }

    out
}

/// Compute the total return of an equal-weight random portfolio
/// using pre-built ticker relatives.
fn random_portfolio_return(
    tickers: &[&String],
    relatives: &HashMap<String, Vec<f64>>,
) -> f64 {
    let valid: Vec<&Vec<f64>> = tickers
        .iter()
        .filter_map(|t| relatives.get(*t))
        .collect();

    if valid.is_empty() { return 0.0; }

    // Final value = average of each ticker's terminal relative value
    let n = valid.len() as f64;
    let terminal: f64 = valid
        .iter()
        .map(|rels| rels.last().copied().unwrap_or(1.0))
        .sum::<f64>()
        / n;

    terminal - 1.0  // convert to return
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