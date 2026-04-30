use anyhow::Result;
use chrono::{Datelike, Duration, NaiveDate};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::info;

use crate::data::{cache::Cache, yahoo::YahooFinance, DataSource};
use crate::universe::builder::Universe;

use super::{mean, std_dev, MarketData, SignalWeights};
use super::momentum::MomentumSignal;
use super::fundamental::FundamentalSignal;
use super::insider::InsiderSignal;
use super::sentiment::SentimentSignal;
use super::{Signal, MacroSnapshot};

// ── Output types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalIcResult {
    pub signal_name: String,
    pub ic_mean: f64,     // mean IC across all test dates
    pub ic_std: f64,      // std dev of IC across test dates
    pub n_dates: usize,   // number of test dates
    pub has_edge: bool,   // IC > 0.05
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonteCarloComparison {
    pub strategy_annualised_return: f64,
    pub random_median_return: f64,
    pub random_p5: f64,
    pub random_p95: f64,
    pub percentile_rank: f64,       // % of random portfolios we beat
    pub n_simulations: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    pub generated_at: String,
    pub train_period: (String, String),
    pub test_period: (String, String),
    pub signal_ic: Vec<SignalIcResult>,
    pub monte_carlo: Option<MonteCarloComparison>,
}

// ── Walk-forward validator ────────────────────────────────────────────────────

pub struct WalkForwardValidator {
    cache: Cache,
    yahoo: YahooFinance,
    weights: SignalWeights,
}

impl WalkForwardValidator {
    pub fn new(cache: Cache) -> Self {
        let yahoo = YahooFinance::new(cache.clone());
        Self {
            cache,
            yahoo,
            weights: SignalWeights::default(),
        }
    }

    /// Run walk-forward validation on the universe.
    /// Train period: 2000-01-01 → train_end.
    /// Test period:  test_start → test_end.
    /// Returns IC per signal and optional Monte Carlo comparison.
    pub async fn validate(
        &self,
        universe: &Universe,
        train_end: NaiveDate,
        test_start: NaiveDate,
        test_end: NaiveDate,
        run_monte_carlo: bool,
    ) -> Result<ValidationReport> {
        info!(
            "Walk-forward validation: train through {}, test {} → {}",
            train_end, test_start, test_end
        );

        let tickers: Vec<String> = universe.tickers();

        // Monthly test dates within [test_start, test_end]
        let test_dates = monthly_dates(test_start, test_end);
        info!("Test dates: {} monthly checkpoints", test_dates.len());

        // Define signals
        let signals: Vec<(&str, &dyn Signal)> = vec![
            ("Momentum",    &MomentumSignal    as &dyn Signal),
            ("Fundamental", &FundamentalSignal as &dyn Signal),
            ("Insider",     &InsiderSignal     as &dyn Signal),
            ("Sentiment",   &SentimentSignal   as &dyn Signal),
        ];

        // Compute IC per signal
        let mut ic_results: Vec<SignalIcResult> = Vec::new();

        for (signal_name, signal) in &signals {
            let ic_series = self
                .compute_signal_ic(signal_name, *signal, &tickers, universe, &test_dates)
                .await;

            let ic_mean = mean(&ic_series);
            let ic_std  = std_dev(&ic_series);
            let has_edge = ic_mean > 0.05;

            info!(
                signal = %signal_name,
                ic_mean = %format!("{:.4}", ic_mean),
                ic_std  = %format!("{:.4}", ic_std),
                has_edge = %has_edge,
                "IC computed"
            );

            ic_results.push(SignalIcResult {
                signal_name: signal_name.to_string(),
                ic_mean,
                ic_std,
                n_dates: ic_series.len(),
                has_edge,
            });
        }

        // Optional Monte Carlo comparison
        let monte_carlo = if run_monte_carlo {
            Some(
                self.run_monte_carlo(&tickers, test_start, test_end, &ic_results)
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!("Monte Carlo failed: {:#}", e);
                        MonteCarloComparison {
                            strategy_annualised_return: 0.0,
                            random_median_return: 0.0,
                            random_p5: 0.0,
                            random_p95: 0.0,
                            percentile_rank: 50.0,
                            n_simulations: 0,
                        }
                    }),
            )
        } else {
            None
        };

        Ok(ValidationReport {
            generated_at: chrono::Local::now().to_rfc3339(),
            train_period: (
                "2000-01-01".to_string(),
                train_end.to_string(),
            ),
            test_period: (
                test_start.to_string(),
                test_end.to_string(),
            ),
            signal_ic: ic_results,
            monte_carlo,
        })
    }

    // ── IC computation ────────────────────────────────────────────────────────

    /// For each test date, compute (signal_score, 30d_forward_return) pairs,
    /// then compute Pearson IC = correlation of scores and returns.
    async fn compute_signal_ic(
        &self,
        _signal_name: &str,
        signal: &dyn Signal,
        tickers: &[String],
        universe: &Universe,
        test_dates: &[NaiveDate],
    ) -> Vec<f64> {
        let mut ic_series = Vec::new();

        for &date in test_dates {
            let forward_date = date + Duration::days(30);

            // Gather (score, forward_return) pairs across all tickers
            let mut pairs: Vec<(f64, f64)> = Vec::new();

            for ticker in tickers {
                // Build minimal market data for this ticker at this date
                let Ok(data) = self.build_minimal_market_data(ticker, date, universe).await else {
                    continue;
                };

                let score = signal.compute(ticker, &data);

                // Forward return: price at date+30d / price at date - 1
                let Ok(fwd_return) = self.compute_forward_return(ticker, date, forward_date).await
                else {
                    continue;
                };

                pairs.push((score, fwd_return));
            }

            if pairs.len() >= 5 {
                let ic = pearson_correlation(&pairs);
                ic_series.push(ic);
            }
        }

        ic_series
    }

    async fn build_minimal_market_data(
        &self,
        ticker: &str,
        as_of: NaiveDate,
        universe: &Universe,
    ) -> Result<MarketData> {
        let price_from = as_of - Duration::days(400);

        let price_bars = self
            .yahoo
            .price_history(ticker, price_from, as_of)
            .await
            .unwrap_or_default();

        let fundamentals = self.yahoo.fundamentals(ticker, as_of).await.ok();

        // Build peer returns from the same industry
        let industry_name = universe
            .all_slots()
            .find(|s| s.ticker == ticker)
            .map(|s| s.industry_name.as_str())
            .unwrap_or("")
            .to_string();

        Ok(MarketData {
            ticker: ticker.to_string(),
            as_of,
            industry_name,
            price_bars,
            fundamentals,
            peer_returns_12m1m: HashMap::new(),
            peer_fundamentals: HashMap::new(),
            insider_trades: Vec::new(),
            news_items: Vec::new(),
            reddit_snapshots: Vec::new(),
            macro_snapshot: MacroSnapshot::neutral(as_of),
        })
    }

    async fn compute_forward_return(
        &self,
        ticker: &str,
        date: NaiveDate,
        forward_date: NaiveDate,
    ) -> Result<f64> {
        let bars = self.yahoo.price_history(ticker, date, forward_date).await?;

        let price_start = bars
            .first()
            .map(|b| b.adj_close)
            .filter(|&p| p > 0.0)
            .ok_or_else(|| anyhow::anyhow!("no start price"))?;

        let price_end = bars
            .last()
            .map(|b| b.adj_close)
            .ok_or_else(|| anyhow::anyhow!("no end price"))?;

        Ok((price_end - price_start) / price_start)
    }

    // ── Monte Carlo ───────────────────────────────────────────────────────────

    async fn run_monte_carlo(
        &self,
        tickers: &[String],
        test_start: NaiveDate,
        test_end: NaiveDate,
        ic_results: &[SignalIcResult],
    ) -> Result<MonteCarloComparison> {
        const N_SIMS: usize = 1_000;
        const N_STOCKS: usize = 10;

        // Compute actual top-10 composite strategy return over test period
        // using IC-weighted signal selection
        let strategy_return = self
            .compute_strategy_return(tickers, test_start, test_end, N_STOCKS)
            .await?;

        let years = (test_end - test_start).num_days() as f64 / 365.25;
        let strategy_annualised = (1.0 + strategy_return).powf(1.0 / years.max(1.0)) - 1.0;

        // Compute returns for all individual tickers over the period
        let ticker_returns = self.compute_all_returns(tickers, test_start, test_end).await;

        // Monte Carlo: 1000 random 10-stock portfolios
        let mut rng_state: u64 = 12345;
        let mut sim_returns: Vec<f64> = Vec::with_capacity(N_SIMS);

        let valid_tickers: Vec<&String> = ticker_returns.keys().collect();
        if valid_tickers.len() < N_STOCKS {
            return Ok(MonteCarloComparison {
                strategy_annualised_return: strategy_annualised,
                random_median_return: 0.0,
                random_p5: 0.0,
                random_p95: 0.0,
                percentile_rank: 50.0,
                n_simulations: 0,
            });
        }

        for _ in 0..N_SIMS {
            // Simple LCG random selection
            let chosen = lcg_sample(&mut rng_state, &valid_tickers, N_STOCKS);
            let avg_return = chosen
                .iter()
                .filter_map(|t| ticker_returns.get(*t))
                .sum::<f64>()
                / N_STOCKS as f64;
            let annualised = (1.0 + avg_return).powf(1.0 / years.max(1.0)) - 1.0;
            sim_returns.push(annualised);
        }

        sim_returns.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let median = percentile(&sim_returns, 50.0);
        let p5     = percentile(&sim_returns, 5.0);
        let p95    = percentile(&sim_returns, 95.0);
        let beat   = sim_returns.iter().filter(|&&r| r < strategy_annualised).count();
        let rank   = beat as f64 / sim_returns.len() as f64 * 100.0;

        Ok(MonteCarloComparison {
            strategy_annualised_return: strategy_annualised,
            random_median_return: median,
            random_p5: p5,
            random_p95: p95,
            percentile_rank: rank,
            n_simulations: N_SIMS,
        })
    }

    async fn compute_strategy_return(
        &self,
        tickers: &[String],
        start: NaiveDate,
        end: NaiveDate,
        n: usize,
    ) -> Result<f64> {
        // Simple proxy: use 12-1m momentum at test_start to pick top-N
        let price_from = start - Duration::days(400);
        let mut momentum_scores: Vec<(String, f64)> = Vec::new();

        for ticker in tickers {
            if let Ok(bars) = self.yahoo.price_history(ticker, price_from, start).await {
                if bars.len() >= 50 {
                    let skip = 21.min(bars.len() / 10);
                    let end_idx = bars.len().saturating_sub(skip + 1);
                    let ret = (bars[end_idx].adj_close - bars[0].adj_close) / bars[0].adj_close.max(1e-9);
                    momentum_scores.push((ticker.clone(), ret));
                }
            }
        }

        momentum_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let top_n: Vec<String> = momentum_scores.into_iter().take(n).map(|(t, _)| t).collect();

        let returns = self.compute_all_returns(&top_n, start, end).await;
        let avg = returns.values().sum::<f64>() / returns.len().max(1) as f64;
        Ok(avg)
    }

    async fn compute_all_returns(
        &self,
        tickers: &[String],
        start: NaiveDate,
        end: NaiveDate,
    ) -> HashMap<String, f64> {
        let mut out = HashMap::new();
        for ticker in tickers {
            if let Ok(bars) = self.yahoo.price_history(ticker, start, end).await {
                if let (Some(first), Some(last)) = (bars.first(), bars.last()) {
                    if first.adj_close > 0.0 {
                        let ret = (last.adj_close - first.adj_close) / first.adj_close;
                        out.insert(ticker.clone(), ret);
                    }
                }
            }
        }
        out
    }
}

// ── Statistical helpers ───────────────────────────────────────────────────────

/// Pearson correlation coefficient between signal scores and forward returns.
fn pearson_correlation(pairs: &[(f64, f64)]) -> f64 {
    if pairs.len() < 2 {
        return 0.0;
    }
    let n = pairs.len() as f64;
    let xs: Vec<f64> = pairs.iter().map(|p| p.0).collect();
    let ys: Vec<f64> = pairs.iter().map(|p| p.1).collect();
    let mx = mean(&xs);
    let my = mean(&ys);

    let cov: f64 = pairs.iter().map(|(x, y)| (x - mx) * (y - my)).sum::<f64>() / n;
    let sx = std_dev(&xs);
    let sy = std_dev(&ys);

    if sx < 1e-9 || sy < 1e-9 {
        return 0.0;
    }
    (cov / (sx * sy)).clamp(-1.0, 1.0)
}

fn monthly_dates(start: NaiveDate, end: NaiveDate) -> Vec<NaiveDate> {
    let mut dates = Vec::new();
    let mut d = start;
    while d <= end {
        dates.push(d);
        // Advance one month (first of next month approach)
        let next_month = if d.month() == 12 {
            NaiveDate::from_ymd_opt(d.year() + 1, 1, 1)
        } else {
            NaiveDate::from_ymd_opt(d.year(), d.month() + 1, 1)
        };
        match next_month {
            Some(nm) => d = nm,
            None => break,
        }
    }
    dates
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Minimal LCG-based random sampling (no `rand` dependency needed here).
fn lcg_sample<'a, T>(state: &mut u64, items: &[&'a T], n: usize) -> Vec<&'a T> {
    let mut selected = Vec::with_capacity(n);
    let len = items.len();
    for _ in 0..n {
        *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let idx = (*state >> 33) as usize % len;
        selected.push(items[idx]);
    }
    selected
}
