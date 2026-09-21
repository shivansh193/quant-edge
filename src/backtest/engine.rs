use anyhow::Result;
use chrono::{Duration, NaiveDate};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use tracing::{info, warn};

use crate::costs::{CostModel, RealizedGain, TaxModel, TaxReport};
use crate::data::cache::Cache;
use crate::data::prices::{PriceSeries, PriceStore};
use crate::data::yahoo::YahooFinance;
use crate::data::DataSource;
use crate::llm::StrategySpec;
use crate::metrics::ic::{self, IcSummary};
use crate::signals::{select_picks, PickingEngine, Ranker, SignalScore};
use crate::universe::builder::Universe;

// ── Config / Result types ─────────────────────────────────────────────────────

/// When decisions turn into fills.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionTiming {
    /// Signal computed on day `t`'s close, filled at day `t+1`'s open.
    /// This is what you can actually do, and the default.
    NextOpen,
    /// Signal and fill both at day `t`'s close. Optimistic (you cannot trade
    /// on a close you have not yet seen); kept only for comparison.
    SameClose,
}

#[derive(Debug, Clone)]
pub struct BacktestConfig {
    pub spec:                  StrategySpec,
    pub start_date:            NaiveDate,
    pub end_date:              NaiveDate,
    pub initial_capital:       f64,
    /// Rebalance every N calendar days (defaults to spec.holding_period_days).
    /// A target that lands on a non-trading day rolls to the next trading day.
    pub rebalance_every_days:  u32,
    pub costs:                 CostModel,
    pub execution:             ExecutionTiming,
    /// Annual risk-free rate used for Sharpe (default 0.0 — set it for a
    /// real excess-return Sharpe).
    pub risk_free_annual:      f64,
    /// Optional tax model applied to realised gains after the run.
    pub tax:                   Option<TaxModel>,
    /// Benchmark ticker; `None` picks ^NSEI or ^GSPC from the universe.
    pub benchmark_ticker:      Option<String>,
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
            costs: CostModel::default_equity(),
            execution: ExecutionTiming::NextOpen,
            risk_free_annual: 0.0,
            tax: None,
            benchmark_ticker: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRecord {
    pub date:        NaiveDate,
    pub ticker:      String,
    pub side:        TradeSide,
    pub shares:      f64,
    /// Actual fill price (after spread, slippage and impact).
    pub price:       f64,
    pub commission:  f64,
    /// Cost of adverse price movement vs. the reference price, in currency.
    #[serde(default)]
    pub slippage_cost: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TradeSide {
    Buy,
    Sell,
}

/// One completed round trip (entry → exit), net of all costs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClosedTrade {
    pub ticker:      String,
    pub entry_date:  NaiveDate,
    pub exit_date:   NaiveDate,
    pub shares:      f64,
    pub entry_price: f64,
    pub exit_price:  f64,
    /// Net profit in currency, after both commissions.
    pub pnl:         f64,
    pub return_pct:  f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BacktestResult {
    pub daily_equity:         Vec<(NaiveDate, f64)>,
    pub trades:               Vec<TradeRecord>,
    pub final_value:          f64,
    pub total_return_pct:     f64,
    pub annualised_return_pct: f64,
    /// Annualised Sharpe of daily excess returns (see `risk_free_annual`).
    pub sharpe_ratio:         f64,
    pub max_drawdown_pct:     f64,
    /// Share of *closed* round trips with positive net P&L.
    pub win_rate_pct:         f64,
    /// Spearman rank IC at each rebalance (composite vs. subsequent return).
    pub signal_ic_per_period: Vec<f64>,

    #[serde(default)]
    pub closed_trades:        Vec<ClosedTrade>,
    #[serde(default)]
    pub ic_summary:           IcSummary,
    #[serde(default)]
    pub benchmark_ticker:     Option<String>,
    #[serde(default)]
    pub benchmark_return_pct: Option<f64>,
    /// `total_return_pct - benchmark_return_pct`.
    #[serde(default)]
    pub alpha_pct:            Option<f64>,
    /// Total commissions + slippage + impact paid, in currency.
    #[serde(default)]
    pub total_costs:          f64,
    /// One-way turnover per year as % of average equity.
    #[serde(default)]
    pub turnover_annualised_pct: f64,
    #[serde(default)]
    pub avg_holding_days:     f64,
    #[serde(default)]
    pub n_rebalances:         usize,
    /// Share of trading days spent 100% in cash (macro gate / no picks).
    #[serde(default)]
    pub cash_days_pct:        f64,
    #[serde(default)]
    pub tax:                  Option<TaxReport>,
    #[serde(default)]
    pub after_tax_return_pct: Option<f64>,
    /// Human-readable warnings about the run (failed rankings, etc.).
    #[serde(default)]
    pub notes:                Vec<String>,
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
        let mut picking_engine = PickingEngine::new(self.cache.clone());
        picking_engine.apply_strategy_spec(&config.spec);

        let prices = self.load_prices(&universe.tickers(), config).await;

        let bench_ticker = config
            .benchmark_ticker
            .clone()
            .unwrap_or_else(|| default_benchmark(universe).to_string());
        let benchmark = self.load_one(&bench_ticker, config).await;

        let mut cfg = config.clone();
        cfg.benchmark_ticker = benchmark.as_ref().map(|_| bench_ticker);
        simulate(&picking_engine, universe, &prices, benchmark.as_ref(), &cfg).await
    }

    /// Prices for the whole window. Uses the cache and only hits the network
    /// for tickers it doesn't already cover.
    async fn load_prices(&self, tickers: &[String], config: &BacktestConfig) -> PriceStore {
        let mut store = PriceStore::new();
        for ticker in tickers {
            if let Some(series) = self.load_one(ticker, config).await {
                store.insert(ticker.clone(), series);
            }
        }
        store
    }

    async fn load_one(&self, ticker: &str, config: &BacktestConfig) -> Option<PriceSeries> {
        let yahoo = YahooFinance::new(self.cache.clone());
        let bars = match yahoo
            .price_history(ticker, config.start_date, config.end_date)
            .await
        {
            Ok(b) => b,
            Err(e) => {
                warn!(ticker = %ticker, "price fetch failed, falling back to cache: {:#}", e);
                self.cache
                    .get_price_bars(ticker, config.start_date, config.end_date)
                    .unwrap_or_default()
            }
        };
        let series = PriceSeries::new(bars);
        (!series.is_empty()).then_some(series)
    }
}

/// `^NSEI` when most of the universe trades on NSE, otherwise `^GSPC`.
pub fn default_benchmark(universe: &Universe) -> &'static str {
    let tickers = universe.tickers();
    let nse = tickers.iter().filter(|t| t.ends_with(".NS")).count();
    if nse * 2 > tickers.len() {
        "^NSEI"
    } else {
        "^GSPC"
    }
}

// ── Simulation core ───────────────────────────────────────────────────────────

/// Run the simulation over pre-loaded prices with any [`Ranker`].
/// Pure with respect to I/O apart from what `ranker` does, so it is unit-testable.
pub async fn simulate(
    ranker: &dyn Ranker,
    universe: &Universe,
    prices: &PriceStore,
    benchmark: Option<&PriceSeries>,
    config: &BacktestConfig,
) -> Result<BacktestResult> {
    anyhow::ensure!(
        config.end_date > config.start_date,
        "end_date must be after start_date"
    );
    anyhow::ensure!(config.initial_capital > 0.0, "initial_capital must be positive");

    let calendar = prices.calendar(config.start_date, config.end_date);
    if calendar.is_empty() {
        anyhow::bail!("No price data available for backtest range");
    }

    let rebalance_set: HashSet<NaiveDate> =
        rebalance_days(&calendar, config.start_date, config.rebalance_every_days)
            .into_iter()
            .collect();

    info!(
        "Backtest {} → {} ({} trading days, {} rebalances, {:?} fills)",
        calendar[0],
        calendar[calendar.len() - 1],
        calendar.len(),
        rebalance_set.len(),
        config.execution,
    );

    let macro_gate = config.spec.filters.macro_filter_enabled;
    let mut book = Book::new(config.initial_capital);
    let mut daily_equity: Vec<(NaiveDate, f64)> = Vec::with_capacity(calendar.len());
    let mut ic_series: Vec<f64> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut prev_scores: Option<(NaiveDate, Vec<SignalScore>)> = None;
    let mut pending: Option<Vec<String>> = None;
    let mut n_rebalances = 0usize;
    let mut cash_days = 0usize;

    for &date in &calendar {
        // 1. Fill orders decided at the previous close.
        if let Some(targets) = pending.take() {
            book.execute(&targets, prices, date, config);
        }

        // 2. Decide, using only information available at this close.
        if rebalance_set.contains(&date) {
            if let Some((prev_date, prev)) = &prev_scores {
                if let Some(v) = period_ic(prev, *prev_date, date, prices) {
                    ic_series.push(v);
                }
            }

            match ranker.rank(universe, date).await {
                Ok(scores) => {
                    let picks = select_picks(
                        &scores,
                        config.spec.top_n,
                        config.spec.filters.min_score,
                        macro_gate,
                    );
                    let targets: Vec<String> = picks.into_iter().map(|p| p.ticker).collect();
                    n_rebalances += 1;
                    match config.execution {
                        ExecutionTiming::NextOpen => pending = Some(targets),
                        ExecutionTiming::SameClose => book.execute(&targets, prices, date, config),
                    }
                    prev_scores = Some((date, scores));
                }
                Err(e) => {
                    warn!("Ranking failed on {}: {:#} — holding existing positions", date, e);
                    notes.push(format!("ranking failed on {date}: {e:#} (positions held)"));
                }
            }
        }

        // 3. Mark to market at the close.
        daily_equity.push((date, book.cash + book.market_value(prices, date)));
        if book.positions.is_empty() {
            cash_days += 1;
        }
    }

    let last_date = calendar[calendar.len() - 1];
    if let Some((prev_date, prev)) = &prev_scores {
        if *prev_date < last_date {
            if let Some(v) = period_ic(prev, *prev_date, last_date, prices) {
                ic_series.push(v);
            }
        }
    }

    // ── Metrics ──────────────────────────────────────────────────────────────
    let final_value = daily_equity.last().map(|(_, v)| *v).unwrap_or(config.initial_capital);
    let total_return_pct = (final_value / config.initial_capital - 1.0) * 100.0;

    let span_days = (last_date - calendar[0]).num_days().max(1) as f64;
    let years = span_days / 365.25;
    let annualised_return_pct = if final_value > 0.0 {
        ((final_value / config.initial_capital).powf(1.0 / years) - 1.0) * 100.0
    } else {
        -100.0
    };

    let daily_returns: Vec<f64> = daily_equity
        .windows(2)
        .map(|w| w[1].1 / w[0].1 - 1.0)
        .collect();
    let sharpe_ratio = compute_sharpe(&daily_returns, config.risk_free_annual);
    let max_drawdown_pct = compute_max_drawdown(&daily_equity);

    let closed = std::mem::take(&mut book.closed);
    let win_rate_pct = win_rate(&closed);
    let avg_holding_days = if closed.is_empty() {
        0.0
    } else {
        closed
            .iter()
            .map(|c| (c.exit_date - c.entry_date).num_days() as f64)
            .sum::<f64>()
            / closed.len() as f64
    };

    let mean_equity = daily_equity.iter().map(|(_, v)| *v).sum::<f64>() / daily_equity.len() as f64;
    let turnover_annualised_pct = if mean_equity > 0.0 {
        (book.traded_notional / 2.0) / mean_equity / years * 100.0
    } else {
        0.0
    };

    let (benchmark_return_pct, alpha_pct) = match benchmark {
        Some(b) => {
            match (b.on_or_before(calendar[0]), b.on_or_before(last_date)) {
                (Some(s), Some(e)) if s.adj_close > 0.0 => {
                    let r = (e.adj_close / s.adj_close - 1.0) * 100.0;
                    (Some(r), Some(total_return_pct - r))
                }
                _ => (None, None),
            }
        }
        None => (None, None),
    };

    let (tax, after_tax_return_pct) = match &config.tax {
        Some(model) => {
            let gains: Vec<RealizedGain> = closed
                .iter()
                .map(|c| RealizedGain {
                    entry_date: c.entry_date,
                    exit_date: c.exit_date,
                    pnl: c.pnl,
                })
                .collect();
            let report = model.assess(&gains);
            let after = (final_value - report.total_tax) / config.initial_capital * 100.0 - 100.0;
            (Some(report), Some(after))
        }
        None => (None, None),
    };

    info!(
        "Backtest complete: final={:.0} total={:.1}% ann={:.1}% sharpe={:.2} mdd={:.1}% costs={:.0}",
        final_value, total_return_pct, annualised_return_pct, sharpe_ratio, max_drawdown_pct, book.total_costs,
    );

    Ok(BacktestResult {
        daily_equity,
        trades: book.trades,
        final_value,
        total_return_pct,
        annualised_return_pct,
        sharpe_ratio,
        max_drawdown_pct,
        win_rate_pct,
        ic_summary: ic::summarize(&ic_series),
        signal_ic_per_period: ic_series,
        closed_trades: closed,
        benchmark_ticker: config.benchmark_ticker.clone(),
        benchmark_return_pct,
        alpha_pct,
        total_costs: book.total_costs,
        turnover_annualised_pct,
        avg_holding_days,
        n_rebalances,
        cash_days_pct: cash_days as f64 / calendar.len() as f64 * 100.0,
        tax,
        after_tax_return_pct,
        notes,
    })
}

// ── Book keeping ──────────────────────────────────────────────────────────────

struct Position {
    shares:           f64,
    entry_date:       NaiveDate,
    entry_fill:       f64,
    entry_commission: f64,
}

struct Book {
    cash:            f64,
    positions:       HashMap<String, Position>,
    trades:          Vec<TradeRecord>,
    closed:          Vec<ClosedTrade>,
    traded_notional: f64,
    total_costs:     f64,
}

impl Book {
    fn new(cash: f64) -> Self {
        Self {
            cash,
            positions: HashMap::new(),
            trades: Vec::new(),
            closed: Vec::new(),
            traded_notional: 0.0,
            total_costs: 0.0,
        }
    }

    /// Mark-to-market at the close of `date`, forward-filling stale prices.
    fn market_value(&self, prices: &PriceStore, date: NaiveDate) -> f64 {
        self.positions
            .iter()
            .map(|(t, p)| {
                let px = prices
                    .get(t)
                    .and_then(|s| s.on_or_before(date))
                    .map_or(0.0, |b| b.adj_close);
                p.shares * px
            })
            .sum()
    }

    /// Move the book to `targets` (equal weight on entry; held names are kept).
    fn execute(
        &mut self,
        targets: &[String],
        prices: &PriceStore,
        date: NaiveDate,
        cfg: &BacktestConfig,
    ) {
        let target_set: HashSet<&str> = targets.iter().map(String::as_str).collect();

        // Sell what is no longer wanted (sorted: deterministic output).
        let mut to_sell: Vec<String> = self
            .positions
            .keys()
            .filter(|t| !target_set.contains(t.as_str()))
            .cloned()
            .collect();
        to_sell.sort();

        for ticker in to_sell {
            let Some(series) = prices.get(&ticker) else { continue };
            let Some(reference) = ref_price(series, date, cfg.execution) else {
                warn!(ticker = %ticker, "no price to sell at {} — position kept", date);
                continue;
            };
            let Some(pos) = self.positions.remove(&ticker) else { continue };

            let notional = pos.shares * reference;
            let (adv, vol) = liquidity(series, date);
            let fill = cfg.costs.execution_price(reference, false, notional, adv, vol);
            let commission = cfg.costs.commission(pos.shares * fill);
            self.cash += pos.shares * fill - commission;

            let slippage = pos.shares * (reference - fill);
            self.total_costs += commission + slippage.max(0.0);
            self.traded_notional += pos.shares * fill;

            let cost_basis = pos.shares * pos.entry_fill + pos.entry_commission;
            let pnl = pos.shares * fill - commission - cost_basis;
            self.closed.push(ClosedTrade {
                ticker: ticker.clone(),
                entry_date: pos.entry_date,
                exit_date: date,
                shares: pos.shares,
                entry_price: pos.entry_fill,
                exit_price: fill,
                pnl,
                return_pct: if cost_basis > 0.0 { pnl / cost_basis * 100.0 } else { 0.0 },
            });
            self.trades.push(TradeRecord {
                date,
                ticker,
                side: TradeSide::Sell,
                shares: pos.shares,
                price: fill,
                commission,
                slippage_cost: slippage,
            });
        }

        if targets.is_empty() {
            return;
        }

        // Size new entries from total equity at the reference prices.
        let held_value: f64 = self
            .positions
            .iter()
            .map(|(t, p)| {
                let px = prices
                    .get(t)
                    .and_then(|s| ref_price(s, date, cfg.execution))
                    .unwrap_or(0.0);
                p.shares * px
            })
            .sum();
        let alloc = (self.cash + held_value) / targets.len() as f64;

        for ticker in targets {
            if self.positions.contains_key(ticker) {
                continue; // keep the existing position
            }
            let Some(series) = prices.get(ticker) else { continue };
            // Must actually trade that day (not halted / no bar).
            if series.on(date).is_none() {
                continue;
            }
            let Some(reference) = ref_price(series, date, cfg.execution) else { continue };

            let spend = alloc.min(self.cash);
            let (adv, vol) = liquidity(series, date);
            let fill = cfg.costs.execution_price(reference, true, spend, adv, vol);
            let commission = cfg.costs.commission(spend);
            let shares = ((spend - commission).max(0.0)) / fill;
            if shares * fill < 1.0 {
                continue;
            }

            self.cash -= shares * fill + commission;
            let slippage = shares * (fill - reference);
            self.total_costs += commission + slippage.max(0.0);
            self.traded_notional += shares * fill;
            self.positions.insert(
                ticker.clone(),
                Position { shares, entry_date: date, entry_fill: fill, entry_commission: commission },
            );
            self.trades.push(TradeRecord {
                date,
                ticker: ticker.clone(),
                side: TradeSide::Buy,
                shares,
                price: fill,
                commission,
                slippage_cost: slippage,
            });
        }
    }
}

/// Price we transact against on `date`: the adjusted open (next-bar fills) or
/// the adjusted close (legacy same-bar fills). Falls back to the last known
/// close if the ticker did not trade that day.
fn ref_price(series: &PriceSeries, date: NaiveDate, timing: ExecutionTiming) -> Option<f64> {
    match series.on(date) {
        Some(b) => {
            let px = match timing {
                ExecutionTiming::NextOpen => b.adj_open(),
                ExecutionTiming::SameClose => b.adj_close,
            };
            Some(if px.is_finite() && px > 0.0 { px } else { b.adj_close })
        }
        None => series.on_or_before(date).map(|b| b.adj_close),
    }
}

/// ADV and volatility from data strictly *before* the fill date.
fn liquidity(series: &PriceSeries, date: NaiveDate) -> (Option<f64>, Option<f64>) {
    let prev = date - Duration::days(1);
    (series.avg_dollar_volume(prev, 20), series.daily_volatility(prev, 20))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Rebalance on the trading calendar: targets are `start + k * every_days`
/// (calendar days) and each rolls forward to the first trading day on or after
/// it. Previously a target that fell on a weekend/holiday was silently skipped.
pub fn rebalance_days(calendar: &[NaiveDate], start: NaiveDate, every_days: u32) -> Vec<NaiveDate> {
    let every = every_days.max(1) as i64;
    let Some(&last) = calendar.last() else {
        return Vec::new();
    };
    let mut out: Vec<NaiveDate> = Vec::new();
    let mut target = start;
    while target <= last {
        let i = calendar.partition_point(|d| *d < target);
        if let Some(&d) = calendar.get(i) {
            if out.last() != Some(&d) {
                out.push(d);
            }
        }
        target += Duration::days(every);
    }
    out
}

/// Spearman rank IC between composite scores at `from` and returns to `to`.
fn period_ic(
    scores: &[SignalScore],
    from: NaiveDate,
    to: NaiveDate,
    prices: &PriceStore,
) -> Option<f64> {
    let stale = Duration::days(7);
    let mut xs = Vec::new();
    let mut ys = Vec::new();
    for s in scores {
        let Some(series) = prices.get(&s.ticker) else { continue };
        let (Some(b0), Some(b1)) = (series.on_or_before(from), series.on_or_before(to)) else {
            continue;
        };
        // Skip names whose data ended (delisted) well before the period did.
        if b0.date < from - stale || b1.date < to - stale {
            continue;
        }
        xs.push(s.composite);
        ys.push(b1.adj_close / b0.adj_close - 1.0);
    }
    if xs.len() < 5 {
        return None;
    }
    ic::spearman(&xs, &ys)
}

/// Annualised Sharpe of daily returns in excess of `risk_free_annual`.
pub fn compute_sharpe(daily_returns: &[f64], risk_free_annual: f64) -> f64 {
    if daily_returns.len() < 2 {
        return 0.0;
    }
    let rf_daily = (1.0 + risk_free_annual).powf(1.0 / 252.0) - 1.0;
    let excess: Vec<f64> = daily_returns.iter().map(|r| r - rf_daily).collect();
    let n = excess.len() as f64;
    let mean = excess.iter().sum::<f64>() / n;
    let variance = excess.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
    let std_dev = variance.sqrt();
    if std_dev < 1e-12 {
        return 0.0;
    }
    (mean / std_dev) * 252.0_f64.sqrt()
}

pub fn compute_max_drawdown(equity: &[(NaiveDate, f64)]) -> f64 {
    let mut peak = f64::NEG_INFINITY;
    let mut max_dd = 0.0_f64;
    for (_, v) in equity {
        if *v > peak {
            peak = *v;
        }
        if peak > 0.0 {
            max_dd = max_dd.max((peak - v) / peak * 100.0);
        }
    }
    max_dd
}

/// Share of closed round trips with positive net P&L.
pub fn win_rate(closed: &[ClosedTrade]) -> f64 {
    if closed.is_empty() {
        return 0.0;
    }
    closed.iter().filter(|c| c.pnl > 0.0).count() as f64 / closed.len() as f64 * 100.0
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::source::PriceBar;
    use crate::signals::{composite_score, SignalAvailability, SignalWeights};
    use crate::universe::builder::{CapFilter, Market, UniverseConfig};
    use async_trait::async_trait;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn universe() -> Universe {
        Universe {
            by_industry: HashMap::new(),
            config: UniverseConfig {
                market: Market::NYSE,
                cap_filter: CapFilter::Mixed,
                n_industries: 0,
                exclude_industry_codes: vec![],
            },
        }
    }

    /// Weekday bars from `start`. `f(i)` gives `(open, close)` for the i-th bar.
    fn weekday_bars(start: &str, n: usize, f: impl Fn(usize) -> (f64, f64)) -> Vec<PriceBar> {
        let mut out = Vec::new();
        let mut date = d(start);
        let mut i = 0;
        while out.len() < n {
            if date.weekday().number_from_monday() <= 5 {
                let (open, close) = f(i);
                out.push(PriceBar {
                    date,
                    open,
                    high: open.max(close),
                    low: open.min(close),
                    close,
                    adj_close: close,
                    volume: 10_000_000,
                });
                i += 1;
            }
            date += Duration::days(1);
        }
        out
    }

    use chrono::Datelike;

    fn store(items: Vec<(&str, Vec<PriceBar>)>) -> PriceStore {
        let mut s = PriceStore::new();
        for (t, bars) in items {
            s.insert(t, PriceSeries::new(bars));
        }
        s
    }

    fn sc(ticker: &str, composite_raw: f64, macro_on: bool) -> SignalScore {
        composite_score(
            ticker, "Ind", composite_raw, 0.0, 0.0, 0.0, 0.0, macro_on,
            &SignalWeights { momentum: 1.0, fundamental: 0.0, insider: 0.0, sentiment: 0.0, pairs: 0.0 },
            &SignalAvailability { momentum: true, ..Default::default() },
        )
    }

    /// Ranker driven by a closure over the as-of date.
    struct FnRanker<F: Fn(NaiveDate) -> Result<Vec<SignalScore>> + Send + Sync>(F);

    #[async_trait]
    impl<F: Fn(NaiveDate) -> Result<Vec<SignalScore>> + Send + Sync> Ranker for FnRanker<F> {
        async fn rank(&self, _u: &Universe, as_of: NaiveDate) -> Result<Vec<SignalScore>> {
            (self.0)(as_of)
        }
    }

    fn config(start: &str, end: &str, every: u32) -> BacktestConfig {
        let mut spec = StrategySpec::default();
        spec.top_n = 1;
        spec.holding_period_days = every;
        let mut c = BacktestConfig::new(spec, d(start), d(end));
        c.costs = CostModel::zero();
        c
    }

    // ── rebalance calendar ────────────────────────────────────────────────────

    #[test]
    fn rebalance_targets_on_weekends_roll_to_next_trading_day() {
        // Weekday-only calendar for January 2024.
        let cal: Vec<NaiveDate> = weekday_bars("2024-01-01", 23, |_| (1.0, 1.0))
            .into_iter()
            .map(|b| b.date)
            .collect();
        let days = rebalance_days(&cal, d("2024-01-01"), 5);
        // Targets: 1/1 Mon, 1/6 Sat→1/8, 1/11 Thu, 1/16 Tue, 1/21 Sun→1/22, 1/26 Fri, 1/31 Wed
        assert_eq!(
            days,
            vec![
                d("2024-01-01"), d("2024-01-08"), d("2024-01-11"), d("2024-01-16"),
                d("2024-01-22"), d("2024-01-26"), d("2024-01-31"),
            ]
        );
    }

    #[test]
    fn rebalance_days_has_no_duplicates_when_targets_collapse() {
        // 1-day cadence over a weekend must not double-count Monday.
        let cal = vec![d("2024-01-05"), d("2024-01-08")];
        assert_eq!(rebalance_days(&cal, d("2024-01-05"), 1), vec![d("2024-01-05"), d("2024-01-08")]);
    }

    #[test]
    fn weekend_start_date_still_rebalances() {
        // Regression: a Saturday start used to yield zero rebalances.
        let cal = vec![d("2024-01-08"), d("2024-01-09")];
        assert_eq!(rebalance_days(&cal, d("2024-01-06"), 30), vec![d("2024-01-08")]);
    }

    // ── execution timing ──────────────────────────────────────────────────────

    /// After the decision close (100) A gaps up to open 120, then trades to 130.
    /// Distinct open and close on the fill day let tests tell them apart.
    fn gap_bars() -> Vec<PriceBar> {
        weekday_bars("2024-01-01", 12, |i| match i {
            0 => (100.0, 100.0),
            1 => (120.0, 130.0),
            _ => (130.0, 130.0),
        })
    }

    fn gap_market() -> PriceStore {
        store(vec![("A", gap_bars())])
    }

    fn pick_a() -> impl Fn(NaiveDate) -> Result<Vec<SignalScore>> + Send + Sync {
        |_| Ok(vec![sc("A", 0.9, true)])
    }

    #[tokio::test]
    async fn next_open_fills_at_the_following_bars_open() {
        // Decision on Mon 1/1 (close 100) → fill Tue 1/2 at its OPEN (120),
        // not the signal-bar close (100) and not that day's close (130).
        let mut cfg = config("2024-01-01", "2024-01-19", 365);
        cfg.execution = ExecutionTiming::NextOpen;
        let r = simulate(&FnRanker(pick_a()), &universe(), &gap_market(), None, &cfg).await.unwrap();
        let buy = &r.trades[0];
        assert_eq!(buy.date, d("2024-01-02"), "must fill the day AFTER the decision");
        assert!((buy.price - 120.0).abs() < 1e-9, "filled at the open, got {}", buy.price);
    }

    #[tokio::test]
    async fn same_close_fills_on_the_signal_bar_and_flatters_results() {
        let prices = gap_market();
        let mut lag = config("2024-01-01", "2024-01-19", 365);
        lag.execution = ExecutionTiming::NextOpen;
        let mut same = lag.clone();
        same.execution = ExecutionTiming::SameClose;

        let r_lag = simulate(&FnRanker(pick_a()), &universe(), &prices, None, &lag).await.unwrap();
        let r_same = simulate(&FnRanker(pick_a()), &universe(), &prices, None, &same).await.unwrap();

        assert_eq!(r_same.trades[0].date, d("2024-01-01"));
        assert!((r_same.trades[0].price - 100.0).abs() < 1e-9);
        // Same-close books the overnight gap (100→130 = +30%); next-open only
        // gets 120→130 = +8.33%.
        assert!((r_same.total_return_pct - 30.0).abs() < 1e-6, "{}", r_same.total_return_pct);
        assert!((r_lag.total_return_pct - (130.0 / 120.0 - 1.0) * 100.0).abs() < 1e-6, "{}", r_lag.total_return_pct);
    }

    // ── closed trades / win rate ──────────────────────────────────────────────

    #[tokio::test]
    async fn win_rate_is_computed_from_closed_round_trips() {
        // A rises steadily, B falls steadily.
        let a = weekday_bars("2024-01-01", 30, |i| (100.0 + i as f64, 100.0 + i as f64));
        let b = weekday_bars("2024-01-01", 30, |i| (100.0 - i as f64, 100.0 - i as f64));
        let prices = store(vec![("A", a), ("B", b)]);

        // Hold A, then B, then A again: sells realise +A, then −B.
        let cfg = config("2024-01-01", "2024-02-09", 7);
        let ranker = FnRanker(|date: NaiveDate| {
            let phase = ((date - d("2024-01-01")).num_days() / 7) % 3;
            Ok(match phase {
                0 | 2 => vec![sc("A", 0.9, true), sc("B", -0.9, true)],
                _ => vec![sc("B", 0.9, true), sc("A", -0.9, true)],
            })
        });
        let r = simulate(&ranker, &universe(), &prices, None, &cfg).await.unwrap();

        assert!(r.closed_trades.len() >= 2, "got {}", r.closed_trades.len());
        let a_trade = r.closed_trades.iter().find(|c| c.ticker == "A").unwrap();
        let b_trade = r.closed_trades.iter().find(|c| c.ticker == "B").unwrap();
        assert!(a_trade.pnl > 0.0, "A rose");
        assert!(b_trade.pnl < 0.0, "B fell");
        // Old code reported ~0% here because entry prices were deleted before lookup.
        assert!(r.win_rate_pct > 0.0 && r.win_rate_pct < 100.0, "win rate {}", r.win_rate_pct);
        let expected = r.closed_trades.iter().filter(|c| c.pnl > 0.0).count() as f64
            / r.closed_trades.len() as f64 * 100.0;
        assert!((r.win_rate_pct - expected).abs() < 1e-9);
    }

    // ── macro gate ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn risk_off_liquidates_to_cash_and_stays_there() {
        let a = weekday_bars("2024-01-01", 40, |i| (100.0 + i as f64, 100.0 + i as f64));
        let prices = store(vec![("A", a)]);
        let cfg = config("2024-01-01", "2024-02-23", 7);
        let ranker = FnRanker(|date: NaiveDate| {
            // Risk-off from mid-January on.
            Ok(vec![sc("A", 0.9, date < d("2024-01-15"))])
        });
        let r = simulate(&ranker, &universe(), &prices, None, &cfg).await.unwrap();

        assert!(r.trades.iter().any(|t| matches!(t.side, TradeSide::Sell)), "must sell on risk-off");
        assert!(r.cash_days_pct > 30.0, "cash days {}", r.cash_days_pct);
        // Equity is flat once in cash.
        let tail: Vec<f64> = r.daily_equity.iter().rev().take(10).map(|(_, v)| *v).collect();
        assert!(tail.windows(2).all(|w| (w[0] - w[1]).abs() < 1e-6));
    }

    #[tokio::test]
    async fn macro_gate_can_be_disabled_in_the_spec() {
        let a = weekday_bars("2024-01-01", 20, |i| (100.0 + i as f64, 100.0 + i as f64));
        let prices = store(vec![("A", a)]);
        let mut cfg = config("2024-01-01", "2024-01-26", 7);
        cfg.spec.filters.macro_filter_enabled = false;
        let ranker = FnRanker(|_| Ok(vec![sc("A", 0.9, false)]));
        let r = simulate(&ranker, &universe(), &prices, None, &cfg).await.unwrap();
        assert!(r.trades.iter().any(|t| matches!(t.side, TradeSide::Buy)));
    }

    // ── failure handling ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn failed_ranking_keeps_positions_and_is_reported() {
        let a = weekday_bars("2024-01-01", 30, |i| (100.0 + i as f64, 100.0 + i as f64));
        let prices = store(vec![("A", a)]);
        let cfg = config("2024-01-01", "2024-02-09", 7);
        let ranker = FnRanker(|date: NaiveDate| {
            if date > d("2024-01-10") { Err(anyhow::anyhow!("network down")) } else { Ok(vec![sc("A", 0.9, true)]) }
        });
        let r = simulate(&ranker, &universe(), &prices, None, &cfg).await.unwrap();
        assert!(!r.notes.is_empty());
        assert!(!r.trades.iter().any(|t| matches!(t.side, TradeSide::Sell)), "must not liquidate on a failed fetch");
    }

    // ── costs, tax, benchmark ─────────────────────────────────────────────────

    #[tokio::test]
    async fn costs_reduce_returns() {
        let a = weekday_bars("2024-01-01", 40, |i| (100.0 + i as f64, 100.0 + i as f64));
        let b = weekday_bars("2024-01-01", 40, |i| (100.0 + i as f64, 100.0 + i as f64));
        let prices = store(vec![("A", a), ("B", b)]);
        let ranker = |date: NaiveDate| {
            // Flip-flop every rebalance to force turnover.
            let k = (date - d("2024-01-01")).num_days() / 7;
            Ok(if k % 2 == 0 { vec![sc("A", 0.9, true)] } else { vec![sc("B", 0.9, true)] })
        };
        let free = config("2024-01-01", "2024-02-23", 7);
        let mut costly = free.clone();
        costly.costs = CostModel { half_spread_bps: 25.0, slippage_bps: 25.0, commission_per_trade: 5.0, ..CostModel::zero() };

        let r0 = simulate(&FnRanker(ranker), &universe(), &prices, None, &free).await.unwrap();
        let r1 = simulate(&FnRanker(ranker), &universe(), &prices, None, &costly).await.unwrap();
        assert!(r1.final_value < r0.final_value);
        assert!(r1.total_costs > 0.0);
        assert_eq!(r0.total_costs, 0.0);
        assert!(r1.turnover_annualised_pct > 0.0);
    }

    #[tokio::test]
    async fn tax_report_is_attached_when_configured() {
        let a = weekday_bars("2024-01-01", 30, |i| (100.0 + i as f64, 100.0 + i as f64));
        let b = weekday_bars("2024-01-01", 30, |i| (100.0 + i as f64, 100.0 + i as f64));
        let prices = store(vec![("A", a), ("B", b)]);
        let ranker = FnRanker(|date: NaiveDate| {
            let k = (date - d("2024-01-01")).num_days() / 7;
            Ok(if k % 2 == 0 { vec![sc("A", 0.9, true)] } else { vec![sc("B", 0.9, true)] })
        });
        let mut cfg = config("2024-01-01", "2024-02-09", 7);
        cfg.tax = Some(TaxModel::us_taxable());
        let r = simulate(&ranker, &universe(), &prices, None, &cfg).await.unwrap();
        let tax = r.tax.expect("tax report");
        assert!(tax.total_tax > 0.0);
        assert!(r.after_tax_return_pct.unwrap() < r.total_return_pct);
    }

    #[tokio::test]
    async fn benchmark_return_and_alpha() {
        let a = weekday_bars("2024-01-01", 30, |i| (100.0 * 1.01f64.powi(i as i32), 100.0 * 1.01f64.powi(i as i32)));
        let prices = store(vec![("A", a)]);
        let bench = PriceSeries::new(weekday_bars("2024-01-01", 30, |i| (100.0 * 1.001f64.powi(i as i32), 100.0 * 1.001f64.powi(i as i32))));
        let cfg = config("2024-01-01", "2024-02-09", 365);
        let r = simulate(&FnRanker(pick_a()), &universe(), &prices, Some(&bench), &cfg).await.unwrap();
        let (b, alpha) = (r.benchmark_return_pct.unwrap(), r.alpha_pct.unwrap());
        assert!(b > 0.0);
        assert!((alpha - (r.total_return_pct - b)).abs() < 1e-9);
        assert!(alpha > 0.0);
    }

    // ── invariants ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn results_are_deterministic() {
        let a = weekday_bars("2024-01-01", 30, |i| (100.0 + i as f64, 101.0 + i as f64));
        let b = weekday_bars("2024-01-01", 30, |i| (100.0 + (i % 5) as f64, 100.0 + (i % 4) as f64));
        let prices = store(vec![("A", a), ("B", b)]);
        let cfg = config("2024-01-01", "2024-02-09", 7);
        let run = || async {
            let ranker = FnRanker(|date: NaiveDate| {
                let k = (date - d("2024-01-01")).num_days() / 7;
                Ok(if k % 2 == 0 { vec![sc("A", 0.9, true), sc("B", 0.1, true)] } else { vec![sc("B", 0.9, true), sc("A", 0.1, true)] })
            });
            simulate(&ranker, &universe(), &prices, None, &cfg).await.unwrap()
        };
        let (r1, r2) = (run().await, run().await);
        assert_eq!(r1.final_value, r2.final_value);
        assert_eq!(r1.trades.len(), r2.trades.len());
    }

    #[tokio::test]
    async fn rejects_inverted_dates_and_empty_data() {
        let prices = gap_market();
        let bad = config("2024-02-01", "2024-01-01", 7);
        assert!(simulate(&FnRanker(pick_a()), &universe(), &prices, None, &bad).await.is_err());
        let empty = PriceStore::new();
        let ok = config("2024-01-01", "2024-02-01", 7);
        assert!(simulate(&FnRanker(pick_a()), &universe(), &empty, None, &ok).await.is_err());
    }

    #[tokio::test]
    async fn equity_starts_at_initial_capital_and_never_goes_negative() {
        let prices = gap_market();
        let cfg = config("2024-01-01", "2024-01-19", 5);
        let r = simulate(&FnRanker(pick_a()), &universe(), &prices, None, &cfg).await.unwrap();
        assert!((r.daily_equity[0].1 - cfg.initial_capital).abs() < 1e-6);
        assert!(r.daily_equity.iter().all(|(_, v)| *v > 0.0));
    }

    // ── metric helpers ────────────────────────────────────────────────────────

    #[test]
    fn max_drawdown_measures_peak_to_trough() {
        let eq: Vec<(NaiveDate, f64)> = [100.0, 120.0, 60.0, 90.0]
            .iter().enumerate()
            .map(|(i, v)| (d("2024-01-01") + Duration::days(i as i64), *v))
            .collect();
        assert!((compute_max_drawdown(&eq) - 50.0).abs() < 1e-9);
    }

    #[test]
    fn sharpe_uses_excess_returns() {
        let rets = [0.010, -0.004, 0.012, 0.002, 0.008, -0.002, 0.006];
        let plain = compute_sharpe(&rets, 0.0);
        let excess = compute_sharpe(&rets, 0.20);
        assert!(plain > 0.0);
        assert!(excess < plain, "a positive risk-free rate must lower Sharpe");
        assert_eq!(compute_sharpe(&[0.0, 0.0, 0.0], 0.0), 0.0);
        assert_eq!(compute_sharpe(&[0.01], 0.0), 0.0);
    }

    #[test]
    fn win_rate_handles_empty_and_mixed() {
        assert_eq!(win_rate(&[]), 0.0);
        let mk = |pnl: f64| ClosedTrade {
            ticker: "X".into(), entry_date: d("2024-01-01"), exit_date: d("2024-01-10"),
            shares: 1.0, entry_price: 1.0, exit_price: 1.0, pnl, return_pct: 0.0,
        };
        assert!((win_rate(&[mk(1.0), mk(-1.0), mk(2.0), mk(-3.0)]) - 50.0).abs() < 1e-9);
    }

    #[test]
    fn period_ic_rewards_a_predictive_ranking() {
        // Composite ordering matches subsequent return ordering exactly.
        let mut items = Vec::new();
        let mut scores = Vec::new();
        for (i, t) in ["A", "B", "C", "D", "E", "F"].iter().enumerate() {
            let growth = 1.0 + 0.01 * (i as f64 + 1.0);
            items.push((*t, weekday_bars("2024-01-01", 10, move |k| (100.0 * growth.powi(k as i32), 100.0 * growth.powi(k as i32)))));
            scores.push(sc(t, -0.9 + 0.3 * i as f64, true));
        }
        let prices = store(items);
        let ic = period_ic(&scores, d("2024-01-01"), d("2024-01-12"), &prices).unwrap();
        assert!(ic > 0.99, "ic {ic}");
    }

    #[test]
    fn period_ic_needs_enough_names() {
        let prices = store(vec![("A", weekday_bars("2024-01-01", 10, |_| (1.0, 1.0)))]);
        assert!(period_ic(&[sc("A", 0.5, true)], d("2024-01-01"), d("2024-01-12"), &prices).is_none());
    }
}
