use anyhow::Result;
use chrono::{Duration, NaiveDate};
use std::collections::HashMap;
use tracing::{info, warn};

use crate::data::cache::Cache;
use crate::llm::StrategySpec;
use crate::signals::{PickingEngine, SignalScore};
use crate::universe::builder::Universe;

// ── Config / Result types ─────────────────────────────────────────────────────

pub struct BacktestConfig {
    pub spec:                  StrategySpec,
    pub start_date:            NaiveDate,
    pub end_date:              NaiveDate,
    pub initial_capital:       f64,
    /// Rebalance every N days (defaults to spec.holding_period_days)
    pub rebalance_every_days:  u32,
    /// One-way slippage in basis points (default 10 = 0.1 %)
    pub slippage_bps:          u32,
    /// Commission per trade in USD (default 1.0)
    pub commission_per_trade:  f64,
}

impl BacktestConfig {
    pub fn new(spec: StrategySpec, start_date: NaiveDate, end_date: NaiveDate) -> Self {
        let rebalance_every_days = spec.holding_period_days;
        Self {
            spec,
            start_date,
            end_date,
            initial_capital: 100_000.0,
            rebalance_every_days,
            slippage_bps: 10,
            commission_per_trade: 1.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TradeRecord {
    pub date:        NaiveDate,
    pub ticker:      String,
    pub side:        TradeSide,
    pub shares:      f64,
    pub price:       f64,
    pub commission:  f64,
}

#[derive(Debug, Clone)]
pub enum TradeSide {
    Buy,
    Sell,
}

pub struct BacktestResult {
    pub daily_equity:         Vec<(NaiveDate, f64)>,
    pub trades:               Vec<TradeRecord>,
    pub final_value:          f64,
    pub total_return_pct:     f64,
    pub annualised_return_pct: f64,
    pub sharpe_ratio:         f64,
    pub max_drawdown_pct:     f64,
    pub win_rate_pct:         f64,
    /// Pearson IC at each rebalance (scores vs. subsequent returns)
    pub signal_ic_per_period: Vec<f64>,
}

// ── Engine ────────────────────────────────────────────────────────────────────

pub struct BacktestEngine {
    cache: Cache,
}

impl BacktestEngine {
    pub fn new(cache: Cache) -> Self {
        Self { cache }
    }

    pub async fn run(
        &self,
        universe: &Universe,
        config: &BacktestConfig,
    ) -> Result<BacktestResult> {
        let total_days = (config.end_date - config.start_date).num_days();
        anyhow::ensure!(total_days > 0, "end_date must be after start_date");

        info!(
            "Backtest {} → {} ({} days, rebalance every {} days)",
            config.start_date,
            config.end_date,
            total_days,
            config.rebalance_every_days,
        );

        // Pre-fetch price bars for all universe tickers over the full range
        let all_tickers = universe.tickers();
        let price_map = self.load_price_map(&all_tickers, config.start_date, config.end_date);

        // Build sorted trading calendar from available price dates
        let calendar = build_calendar(&price_map, config.start_date, config.end_date);
        if calendar.is_empty() {
            anyhow::bail!("No price data available for backtest range");
        }

        // Compute rebalance dates — first date plus every N calendar days after
        let rebalance_dates = rebalance_schedule(
            config.start_date,
            config.end_date,
            config.rebalance_every_days,
        );

        let mut cash = config.initial_capital;
        let mut positions: HashMap<String, f64> = HashMap::new(); // ticker → shares
        let mut entry_prices: HashMap<String, f64> = HashMap::new();
        let mut trades: Vec<TradeRecord> = Vec::new();
        let mut daily_equity: Vec<(NaiveDate, f64)> = Vec::new();
        let mut signal_ic_per_period: Vec<f64> = Vec::new();

        // Scores at last rebalance — used to compute IC against next-period returns
        let mut prev_scores: Option<(NaiveDate, Vec<SignalScore>)> = None;

        let mut picking_engine = PickingEngine::new(self.cache.clone());
        picking_engine.apply_strategy_spec(&config.spec);

        for &date in &calendar {
            // ── Rebalance if due ─────────────────────────────────────────────
            if rebalance_dates.contains(&date) {
                // Before rebalancing: compute IC from previous period
                if let Some((prev_date, ref prev)) = prev_scores {
                    if let Some(ic) = compute_period_ic(prev, prev_date, date, &price_map) {
                        signal_ic_per_period.push(ic);
                    }
                }

                match picking_engine.rank_universe(universe, date).await {
                    Ok(scores) => {
                        let top_picks = filter_picks(&scores, &config.spec);
                        let portfolio_value = portfolio_value(&positions, &price_map, date) + cash;

                        let new_tickers: std::collections::HashSet<String> =
                            top_picks.iter().map(|s| s.ticker.clone()).collect();
                        let held_tickers: Vec<String> = positions.keys().cloned().collect();

                        // Sell positions not in new picks
                        for ticker in &held_tickers {
                            if !new_tickers.contains(ticker) {
                                if let Some(&shares) = positions.get(ticker) {
                                    let price = last_price(&price_map, ticker, date);
                                    if price > 0.0 {
                                        let fill = price * (1.0 - config.slippage_bps as f64 / 10_000.0);
                                        let proceeds = shares * fill - config.commission_per_trade;
                                        cash += proceeds;
                                        trades.push(TradeRecord {
                                            date,
                                            ticker: ticker.clone(),
                                            side: TradeSide::Sell,
                                            shares,
                                            price: fill,
                                            commission: config.commission_per_trade,
                                        });
                                        positions.remove(ticker);
                                        entry_prices.remove(ticker);
                                    }
                                }
                            }
                        }

                        // Buy new picks (equal weight)
                        if !top_picks.is_empty() {
                            let alloc_per_ticker = portfolio_value / top_picks.len() as f64;
                            for score in &top_picks {
                                if positions.contains_key(&score.ticker) {
                                    continue; // already held — keep existing position
                                }
                                let price = last_price(&price_map, &score.ticker, date);
                                if price <= 0.0 {
                                    continue;
                                }
                                let fill = price * (1.0 + config.slippage_bps as f64 / 10_000.0);
                                let spend = (alloc_per_ticker - config.commission_per_trade).max(0.0);
                                let shares = spend / fill;
                                if shares > 0.0 {
                                    let cost = shares * fill + config.commission_per_trade;
                                    if cost <= cash {
                                        cash -= cost;
                                        positions.insert(score.ticker.clone(), shares);
                                        entry_prices.insert(score.ticker.clone(), fill);
                                        trades.push(TradeRecord {
                                            date,
                                            ticker: score.ticker.clone(),
                                            side: TradeSide::Buy,
                                            shares,
                                            price: fill,
                                            commission: config.commission_per_trade,
                                        });
                                    }
                                }
                            }
                        }

                        prev_scores = Some((date, scores));
                    }
                    Err(e) => {
                        warn!("Picking engine failed on {}: {:#}", date, e);
                    }
                }
            }

            // ── Mark to market ───────────────────────────────────────────────
            let equity = portfolio_value(&positions, &price_map, date) + cash;
            daily_equity.push((date, equity));
        }

        // Compute IC for final period
        if let Some((prev_date, ref prev)) = prev_scores {
            if let Some(ic) = compute_period_ic(prev, prev_date, config.end_date, &price_map) {
                signal_ic_per_period.push(ic);
            }
        }

        // ── Metrics ──────────────────────────────────────────────────────────
        let final_value = daily_equity.last().map(|(_, v)| *v).unwrap_or(config.initial_capital);
        let total_return_pct = (final_value / config.initial_capital - 1.0) * 100.0;

        let years = total_days as f64 / 365.0;
        let annualised_return_pct = if years > 0.0 {
            ((final_value / config.initial_capital).powf(1.0 / years) - 1.0) * 100.0
        } else {
            0.0
        };

        let daily_returns: Vec<f64> = daily_equity
            .windows(2)
            .map(|w| w[1].1 / w[0].1 - 1.0)
            .collect();

        let sharpe_ratio = compute_sharpe(&daily_returns);
        let max_drawdown_pct = compute_max_drawdown(&daily_equity);
        let win_rate_pct = compute_win_rate(&trades, &entry_prices, &price_map);

        info!(
            "Backtest complete: final={:.0} total={:.1}% ann={:.1}% sharpe={:.2} mdd={:.1}%",
            final_value,
            total_return_pct,
            annualised_return_pct,
            sharpe_ratio,
            max_drawdown_pct,
        );

        Ok(BacktestResult {
            daily_equity,
            trades,
            final_value,
            total_return_pct,
            annualised_return_pct,
            sharpe_ratio,
            max_drawdown_pct,
            win_rate_pct,
            signal_ic_per_period,
        })
    }

    /// Load price bars for all tickers into a nested HashMap for O(1) lookup.
    fn load_price_map(
        &self,
        tickers: &[String],
        from: NaiveDate,
        to: NaiveDate,
    ) -> HashMap<String, HashMap<NaiveDate, f64>> {
        let mut map: HashMap<String, HashMap<NaiveDate, f64>> = HashMap::new();
        for ticker in tickers {
            match self.cache.get_price_bars(ticker, from, to) {
                Ok(bars) => {
                    let inner: HashMap<NaiveDate, f64> =
                        bars.into_iter().map(|b| (b.date, b.adj_close)).collect();
                    if !inner.is_empty() {
                        map.insert(ticker.clone(), inner);
                    }
                }
                Err(_) => {}
            }
        }
        map
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Generate rebalance dates: start_date + every rebalance_every_days thereafter.
fn rebalance_schedule(
    start: NaiveDate,
    end: NaiveDate,
    every_days: u32,
) -> std::collections::HashSet<NaiveDate> {
    let mut dates = std::collections::HashSet::new();
    let mut current = start;
    while current <= end {
        dates.insert(current);
        current += Duration::days(every_days as i64);
    }
    dates
}

/// Sorted list of all calendar dates that have at least one ticker with a price bar.
fn build_calendar(
    price_map: &HashMap<String, HashMap<NaiveDate, f64>>,
    from: NaiveDate,
    to: NaiveDate,
) -> Vec<NaiveDate> {
    let mut dates: std::collections::BTreeSet<NaiveDate> = std::collections::BTreeSet::new();
    for inner in price_map.values() {
        for &date in inner.keys() {
            if date >= from && date <= to {
                dates.insert(date);
            }
        }
    }
    dates.into_iter().collect()
}

/// Last known price for a ticker on or before `date`.
fn last_price(
    price_map: &HashMap<String, HashMap<NaiveDate, f64>>,
    ticker: &str,
    date: NaiveDate,
) -> f64 {
    let Some(inner) = price_map.get(ticker) else { return 0.0 };
    // Find latest date ≤ requested date
    inner
        .iter()
        .filter(|(&d, _)| d <= date)
        .max_by_key(|(&d, _)| d)
        .map(|(_, &p)| p)
        .unwrap_or(0.0)
}

/// Mark-to-market value of all positions.
fn portfolio_value(
    positions: &HashMap<String, f64>,
    price_map: &HashMap<String, HashMap<NaiveDate, f64>>,
    date: NaiveDate,
) -> f64 {
    positions
        .iter()
        .map(|(ticker, &shares)| shares * last_price(price_map, ticker, date))
        .sum()
}

/// Apply spec filters (top_n, min_score) and return the tickers to hold.
fn filter_picks(scores: &[SignalScore], spec: &StrategySpec) -> Vec<SignalScore> {
    let mut filtered: Vec<SignalScore> = scores
        .iter()
        .filter(|s| {
            if let Some(min) = spec.filters.min_score {
                s.composite >= min
            } else {
                true
            }
        })
        .cloned()
        .collect();

    filtered.sort_by(|a, b| b.composite.partial_cmp(&a.composite).unwrap_or(std::cmp::Ordering::Equal));
    filtered.truncate(spec.top_n);
    filtered
}

/// Pearson IC: correlation between composite scores at `from` and forward returns to `to`.
fn compute_period_ic(
    scores: &[SignalScore],
    from: NaiveDate,
    to: NaiveDate,
    price_map: &HashMap<String, HashMap<NaiveDate, f64>>,
) -> Option<f64> {
    let pairs: Vec<(f64, f64)> = scores
        .iter()
        .filter_map(|s| {
            let p0 = last_price(price_map, &s.ticker, from);
            let p1 = last_price(price_map, &s.ticker, to);
            if p0 > 0.0 && p1 > 0.0 {
                Some((s.composite, (p1 / p0 - 1.0) * 100.0))
            } else {
                None
            }
        })
        .collect();

    if pairs.len() < 5 {
        return None;
    }

    Some(pearson(&pairs))
}

fn pearson(pairs: &[(f64, f64)]) -> f64 {
    let n = pairs.len() as f64;
    let mx = pairs.iter().map(|(x, _)| x).sum::<f64>() / n;
    let my = pairs.iter().map(|(_, y)| y).sum::<f64>() / n;

    let num: f64 = pairs.iter().map(|(x, y)| (x - mx) * (y - my)).sum();
    let dx: f64 = pairs.iter().map(|(x, _)| (x - mx).powi(2)).sum::<f64>().sqrt();
    let dy: f64 = pairs.iter().map(|(_, y)| (y - my).powi(2)).sum::<f64>().sqrt();

    if dx < 1e-9 || dy < 1e-9 {
        0.0
    } else {
        (num / (dx * dy)).clamp(-1.0, 1.0)
    }
}

fn compute_sharpe(daily_returns: &[f64]) -> f64 {
    if daily_returns.len() < 2 {
        return 0.0;
    }
    let n = daily_returns.len() as f64;
    let mean = daily_returns.iter().sum::<f64>() / n;
    let variance = daily_returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
    let std_dev = variance.sqrt();
    if std_dev < 1e-9 {
        return 0.0;
    }
    (mean / std_dev) * (252.0_f64).sqrt()
}

fn compute_max_drawdown(equity: &[(NaiveDate, f64)]) -> f64 {
    let mut peak = f64::NEG_INFINITY;
    let mut max_dd = 0.0_f64;
    for (_, v) in equity {
        if *v > peak {
            peak = *v;
        }
        if peak > 0.0 {
            let dd = (peak - v) / peak * 100.0;
            if dd > max_dd {
                max_dd = dd;
            }
        }
    }
    max_dd
}

fn compute_win_rate(
    trades: &[TradeRecord],
    entry_prices: &HashMap<String, f64>,
    price_map: &HashMap<String, HashMap<NaiveDate, f64>>,
) -> f64 {
    let sells: Vec<&TradeRecord> = trades.iter().filter(|t| matches!(t.side, TradeSide::Sell)).collect();
    if sells.is_empty() {
        return 0.0;
    }
    let wins = sells.iter().filter(|t| {
        entry_prices
            .get(&t.ticker)
            .map(|&entry| t.price > entry)
            .unwrap_or(false)
    }).count();
    wins as f64 / sells.len() as f64 * 100.0
}
