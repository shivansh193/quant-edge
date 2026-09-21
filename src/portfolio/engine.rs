use anyhow::{Context, Result};
use chrono::{Datelike, Duration, NaiveDate};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::info;

use crate::data::prices::PriceSeries;
use crate::data::source::DataSource;
use crate::roles::classifier::{IndustryRoster, Role};
use crate::universe::builder::Universe;
use super::rebalancer::{Rebalancer, SwapEvent};
use super::weights::{WeightMap, WeightMode};

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, clap::ValueEnum, PartialEq)]
pub enum RebalanceFrequency {
    Daily,
    Weekly,
    Monthly,
    Quarterly,
    Annual,
}

impl RebalanceFrequency {
    /// Calendar dates on which a rebalance is *due* after `start`, up to `end`.
    ///
    /// Each target is computed from `start` (never cumulatively, so month-end
    /// starts do not drift) and clamped to the month's last day. A target that
    /// lands on a weekend/holiday is honoured by the caller on the next trading
    /// day. The old day-of-month equality check silently skipped any rebalance
    /// whose day did not exist or fell on a weekend.
    pub fn checkpoints(&self, start: NaiveDate, end: NaiveDate) -> Vec<NaiveDate> {
        use chrono::Months;
        let mut out = Vec::new();
        match self {
            RebalanceFrequency::Daily | RebalanceFrequency::Weekly => {
                let step = if matches!(self, RebalanceFrequency::Daily) { 1 } else { 7 };
                let mut d = start + chrono::Duration::days(step);
                while d <= end {
                    out.push(d);
                    d += chrono::Duration::days(step);
                }
            }
            RebalanceFrequency::Monthly
            | RebalanceFrequency::Quarterly
            | RebalanceFrequency::Annual => {
                let months = match self {
                    RebalanceFrequency::Monthly => 1,
                    RebalanceFrequency::Quarterly => 3,
                    _ => 12,
                };
                let mut k = 1u32;
                while let Some(d) = start.checked_add_months(Months::new(months * k)) {
                    if d > end {
                        break;
                    }
                    out.push(d);
                    k += 1;
                }
            }
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct SimulationConfig {
    pub start_date:      NaiveDate,
    pub end_date:        NaiveDate,
    pub initial_capital: f64,
    pub rebalance_freq:  RebalanceFrequency,
    pub weight_mode:     WeightMode,
    pub active_roles:    Vec<Role>,
    pub benchmark_ticker: String,   // e.g. "^NSEI" or "^GSPC"
    /// One-way cost applied to traded notional at each (re)allocation, in bps.
    pub transaction_cost_bps: f64,
}

// ── Result types ──────────────────────────────────────────────────────────────

/// One data point in the portfolio value time series.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioSnapshot {
    pub date:            NaiveDate,
    pub portfolio_value: f64,
    pub benchmark_value: f64,
    pub holdings:        HashMap<String, f64>, // ticker → current value
}

/// Per-role contribution to total return.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolePerformance {
    pub role:           Role,
    pub total_return:   f64,   // arithmetic return across all holdings in role
    pub avg_weight:     f64,   // average portfolio weight across sim period
    pub swap_count:     usize, // how often this role changed hands
}

/// Per-industry contribution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndustryPerformance {
    pub industry_code:  u32,
    pub industry_name:  String,
    pub total_return:   f64,
}

#[derive(Debug)]
pub struct SimulationResult {
    pub snapshots:          Vec<PortfolioSnapshot>,
    pub swap_log:           Vec<SwapEvent>,
    pub role_performance:   Vec<RolePerformance>,
    pub industry_perf:      Vec<IndustryPerformance>,
    pub config:             SimulationConfig,
    /// Buy-and-hold total return of *every* universe ticker over the window —
    /// the population the Monte Carlo baseline samples from.
    pub universe_returns:   HashMap<String, f64>,
}

impl SimulationResult {
    /// Final portfolio value.
    pub fn final_value(&self) -> f64 {
        self.snapshots
            .last()
            .map(|s| s.portfolio_value)
            .unwrap_or(0.0)
    }

    /// Total return over the simulation period.
    pub fn total_return(&self) -> f64 {
        let start = self.config.initial_capital;
        (self.final_value() - start) / start
    }

    /// Final benchmark value.
    pub fn benchmark_final(&self) -> f64 {
        self.snapshots
            .last()
            .map(|s| s.benchmark_value)
            .unwrap_or(0.0)
    }
}

// ── Engine ────────────────────────────────────────────────────────────────────

pub struct SimulationEngine<'a> {
    source: &'a dyn DataSource,
    config: SimulationConfig,
}

impl<'a> SimulationEngine<'a> {
    pub fn new(source: &'a dyn DataSource, config: SimulationConfig) -> Self {
        Self { source, config }
    }

    pub async fn run(&self, universe: &Universe) -> Result<SimulationResult> {
        let cfg = &self.config;

        info!(
            "Starting simulation {} → {}  capital={:.0}  rebalance={:?}",
            cfg.start_date, cfg.end_date, cfg.initial_capital, cfg.rebalance_freq
        );

        // ── Pre-fetch all price data ──────────────────────────────────────────
        // Fetch price bars for every ticker + benchmark over the full window.
        // Everything lands in SQLite cache — day loop reads from memory.

        let all_tickers = universe.tickers();
        let price_data = self
            .prefetch_prices(&all_tickers, cfg.start_date, cfg.end_date)
            .await?;

        let benchmark_bars = self
            .source
            .price_history(&cfg.benchmark_ticker, cfg.start_date, cfg.end_date)
            .await
            .context("Benchmark price fetch failed")?;

        let benchmark = PriceSeries::new(benchmark_bars);
        anyhow::ensure!(
            !benchmark.is_empty(),
            "No benchmark price data for {} in the window",
            cfg.benchmark_ticker
        );

        // ── Initial classification ────────────────────────────────────────────

        let rebalancer = Rebalancer::new(
            self.source,
            cfg.active_roles.clone(),
            cfg.weight_mode.clone(),
        );

        let initial = rebalancer
            .rebalance(universe, cfg.start_date, &HashMap::new())
            .await?;

        let mut current_rosters: HashMap<u32, IndustryRoster> = initial.rosters;
        let mut current_weights: WeightMap                     = initial.weights;
        let mut all_swaps:       Vec<SwapEvent>                = initial.swap_events;

        // ── Holdings: ticker → number of shares ──────────────────────────────

        // The initial purchase is turnover too: pay the cost on the full amount.
        let initial_cost = cfg.initial_capital * cfg.transaction_cost_bps / 10_000.0;
        let mut holdings: HashMap<String, f64> = self.allocate(
            &current_weights,
            cfg.initial_capital - initial_cost,
            &price_data,
            cfg.start_date,
        );

        // ── Tracking structures ───────────────────────────────────────────────

        let mut snapshots:       Vec<PortfolioSnapshot>            = Vec::new();
        let mut role_swap_count: HashMap<Role, usize>              = HashMap::new();
        let mut role_weight_sum: HashMap<Role, f64>                = HashMap::new();
        let _role_return_sum: HashMap<Role, f64>                = HashMap::new();
        let _industry_returns: HashMap<u32, (String, f64, f64)> = HashMap::new();
        // industry_code → (name, start_value, current_value)

        // First benchmark close on/after the start. (The old code fell back to
        // 1.0 when the start date was not a trading day, which made the
        // benchmark line — and therefore alpha — nonsense.)
        let benchmark_start = benchmark
            .on_or_before(cfg.start_date)
            .or_else(|| benchmark.strictly_after(cfg.start_date))
            .map(|b| b.adj_close)
            .filter(|p| *p > 0.0)
            .context("Benchmark has no usable start price")?;

        // Rebalances due after the start, on the calendar (see `checkpoints`).
        let checkpoints = cfg.rebalance_freq.checkpoints(cfg.start_date, cfg.end_date);
        let mut next_checkpoint = 0usize;

        // ── Day loop ──────────────────────────────────────────────────────────

        let mut date = cfg.start_date;

        while date <= cfg.end_date {
            // Skip weekends — markets closed
            if is_weekend(date) {
                date = date.succ_opt().unwrap_or(date);
                continue;
            }

            // A checkpoint is due once we reach (or pass) its calendar date.
            let mut rebalance_due = false;
            while next_checkpoint < checkpoints.len() && checkpoints[next_checkpoint] <= date {
                rebalance_due = true;
                next_checkpoint += 1;
            }

            if date > cfg.start_date && rebalance_due {
                let result = rebalancer
                    .rebalance(universe, date, &current_rosters)
                    .await?;

                // Track swap counts per role
                for event in &result.swap_events {
                    *role_swap_count.entry(event.role.clone()).or_insert(0) += 1;
                }

                all_swaps.extend(result.swap_events);
                current_rosters = result.rosters;
                current_weights = result.weights;

                // Re-allocate holdings at new weights using current portfolio
                // value, net of the cost of the turnover this implies.
                let portfolio_value =
                    self.compute_portfolio_value(&holdings, &price_data, date);
                let target = self.allocate(&current_weights, portfolio_value, &price_data, date);
                let cost = self.turnover_cost(&holdings, &target, &price_data, date);
                holdings = if cost > 0.0 && portfolio_value > cost {
                    self.allocate(&current_weights, portfolio_value - cost, &price_data, date)
                } else {
                    target
                };
            }

            // ── Compute portfolio value for today ─────────────────────────────

            let portfolio_value =
                self.compute_portfolio_value(&holdings, &price_data, date);

            // Forward-fill from the most recent benchmark close.
            let benchmark_price = benchmark
                .on_or_before(date)
                .map_or(benchmark_start, |b| b.adj_close);

            let benchmark_value =
                (benchmark_price / benchmark_start) * cfg.initial_capital;

            // Holdings snapshot (ticker → dollar value)
            let holdings_value: HashMap<String, f64> = holdings
                .iter()
                .filter_map(|(ticker, &shares)| {
                    let price = self.last_known_ticker_price(&price_data, ticker, date)?;
                    Some((ticker.clone(), shares * price))
                })
                .collect();

            // ── Accumulate role-level stats ───────────────────────────────────

            for (role, weight) in self.weights_by_role(&current_weights, &current_rosters) {
                *role_weight_sum.entry(role).or_insert(0.0) += weight;
            }

            snapshots.push(PortfolioSnapshot {
                date,
                portfolio_value,
                benchmark_value,
                holdings: holdings_value,
            });

            date = date.succ_opt().unwrap_or(date);
        }

        // ── Post-simulation analytics ─────────────────────────────────────────

        let n_days = snapshots.len() as f64;

        let role_performance: Vec<RolePerformance> = cfg
            .active_roles
            .iter()
            .map(|role| {
                let avg_weight = role_weight_sum.get(role).copied().unwrap_or(0.0) / n_days;
                let swap_count = role_swap_count.get(role).copied().unwrap_or(0);
                RolePerformance {
                    role: role.clone(),
                    total_return: self.compute_role_return(role, &snapshots, &current_rosters),
                    avg_weight,
                    swap_count,
                }
            })
            .collect();

        let industry_perf: Vec<IndustryPerformance> =
            self.compute_industry_performance(&snapshots, &current_rosters);

        info!(
            "Simulation complete — {} trading days, {} swaps, final value={:.2}",
            snapshots.len(),
            all_swaps.len(),
            snapshots.last().map(|s| s.portfolio_value).unwrap_or(0.0)
        );

        let universe_returns = universe_buy_and_hold_returns(&price_data);

        Ok(SimulationResult {
            snapshots,
            swap_log: all_swaps,
            role_performance,
            industry_perf,
            config: self.config.clone(),
            universe_returns,
        })
    }

    // ── Price helpers ─────────────────────────────────────────────────────────

    /// Pre-fetch all tickers into cache, return as date-keyed maps.
    async fn prefetch_prices(
        &self,
        tickers: &[String],
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<HashMap<String, HashMap<NaiveDate, f64>>> {
        let batch = self
            .source
            .price_history_batch(tickers, from, to)
            .await?;

        let mut out = HashMap::new();
        for (ticker, bars) in batch {
            let map: HashMap<NaiveDate, f64> =
                bars.iter().map(|b| (b.date, b.adj_close)).collect();
            out.insert(ticker, map);
        }
        Ok(out)
    }

    fn compute_portfolio_value(
        &self,
        holdings: &HashMap<String, f64>,
        price_data: &HashMap<String, HashMap<NaiveDate, f64>>,
        date: NaiveDate,
    ) -> f64 {
        holdings
            .iter()
            .map(|(ticker, &shares)| {
                let price = self
                    .last_known_ticker_price(price_data, ticker, date)
                    .unwrap_or(0.0);
                shares * price
            })
            .sum()
    }

    /// Allocate capital into share counts based on weights and current prices.
    fn allocate(
        &self,
        weights: &WeightMap,
        capital: f64,
        price_data: &HashMap<String, HashMap<NaiveDate, f64>>,
        date: NaiveDate,
    ) -> HashMap<String, f64> {
        weights
            .iter()
            .filter_map(|(ticker, &weight)| {
                let price = self.last_known_ticker_price(price_data, ticker, date)?;
                if price <= 0.0 {
                    return None;
                }
                let shares = (capital * weight) / price;
                Some((ticker.clone(), shares))
            })
            .collect()
    }

    /// Forward-fill: return most recent known price on or before `date`.
    fn last_known_ticker_price(
        &self,
        price_data: &HashMap<String, HashMap<NaiveDate, f64>>,
        ticker: &str,
        date: NaiveDate,
    ) -> Option<f64> {
        let map = price_data.get(ticker)?;
        // Walk back up to 5 trading days to handle holidays/gaps
        for offset in 0..=5i64 {
            let d = date - Duration::days(offset);
            if let Some(&price) = map.get(&d) {
                return Some(price);
            }
        }
        None
    }

    /// Cost of moving from `old` share counts to `new` share counts:
    /// `transaction_cost_bps` × Σ |Δ value| across every ticker touched.
    fn turnover_cost(
        &self,
        old: &HashMap<String, f64>,
        new: &HashMap<String, f64>,
        price_data: &HashMap<String, HashMap<NaiveDate, f64>>,
        date: NaiveDate,
    ) -> f64 {
        if self.config.transaction_cost_bps <= 0.0 {
            return 0.0;
        }
        let tickers: std::collections::HashSet<&String> = old.keys().chain(new.keys()).collect();
        let traded: f64 = tickers
            .into_iter()
            .map(|t| {
                let px = self.last_known_ticker_price(price_data, t, date).unwrap_or(0.0);
                let d_shares = new.get(t).copied().unwrap_or(0.0) - old.get(t).copied().unwrap_or(0.0);
                (d_shares * px).abs()
            })
            .sum();
        traded * self.config.transaction_cost_bps / 10_000.0
    }

    // ── Analytics helpers ─────────────────────────────────────────────────────

    /// Map role → sum of weights of tickers currently assigned to that role.
    fn weights_by_role(
        &self,
        weights: &WeightMap,
        rosters: &HashMap<u32, IndustryRoster>,
    ) -> HashMap<Role, f64> {
        let mut out: HashMap<Role, f64> = HashMap::new();

        for roster in rosters.values() {
            for (role, assignment) in &roster.assignments {
                if let Some(&w) = weights.get(&assignment.ticker) {
                    *out.entry(role.clone()).or_insert(0.0) += w;
                }
            }
        }

        out
    }

    /// Approximate per-role return: weighted average of ticker returns
    /// for tickers currently assigned to that role in the final roster.
    fn compute_role_return(
        &self,
        role: &Role,
        snapshots: &[PortfolioSnapshot],
        rosters: &HashMap<u32, IndustryRoster>,
    ) -> f64 {
        let role_tickers: Vec<String> = rosters
            .values()
            .filter_map(|r| r.assignments.get(role))
            .map(|a| a.ticker.clone())
            .collect();

        if role_tickers.is_empty() || snapshots.len() < 2 {
            return 0.0;
        }

        let first = &snapshots[0];
        let last  = &snapshots[snapshots.len() - 1];

        let start_val: f64 = role_tickers
            .iter()
            .filter_map(|t| first.holdings.get(t))
            .sum();

        let end_val: f64 = role_tickers
            .iter()
            .filter_map(|t| last.holdings.get(t))
            .sum();

        if start_val <= 0.0 { 0.0 } else { (end_val - start_val) / start_val }
    }

    fn compute_industry_performance(
        &self,
        snapshots: &[PortfolioSnapshot],
        rosters: &HashMap<u32, IndustryRoster>,
    ) -> Vec<IndustryPerformance> {
        if snapshots.len() < 2 {
            return Vec::new();
        }

        let first = &snapshots[0];
        let last  = &snapshots[snapshots.len() - 1];

        let mut perf: Vec<IndustryPerformance> = rosters
            .values()
            .map(|roster| {
                let tickers: Vec<&str> = roster
                    .assignments
                    .values()
                    .map(|a| a.ticker.as_str())
                    .collect();

                let start_val: f64 = tickers
                    .iter()
                    .filter_map(|t| first.holdings.get(*t))
                    .sum();

                let end_val: f64 = tickers
                    .iter()
                    .filter_map(|t| last.holdings.get(*t))
                    .sum();

                let total_return = if start_val > 0.0 {
                    (end_val - start_val) / start_val
                } else {
                    0.0
                };

                IndustryPerformance {
                    industry_code: roster.industry_code,
                    industry_name: roster.industry_name.clone(),
                    total_return,
                }
            })
            .collect();

        // Sort by return descending
        perf.sort_by(|a, b| b.total_return.partial_cmp(&a.total_return).unwrap());
        perf
    }
}

/// Buy-and-hold total return of each ticker between its first and last price
/// in the window. This is the population the Monte Carlo baseline samples.
fn universe_buy_and_hold_returns(
    price_data: &HashMap<String, HashMap<NaiveDate, f64>>,
) -> HashMap<String, f64> {
    price_data
        .iter()
        .filter_map(|(ticker, prices)| {
            let first = prices.iter().min_by_key(|(d, _)| **d)?.1;
            let last = prices.iter().max_by_key(|(d, _)| **d)?.1;
            (*first > 0.0 && last.is_finite()).then(|| (ticker.clone(), last / first - 1.0))
        })
        .collect()
}

// ── Calendar helpers ──────────────────────────────────────────────────────────

fn is_weekend(date: NaiveDate) -> bool {
    use chrono::Weekday;
    matches!(date.weekday(), Weekday::Sat | Weekday::Sun)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn monthly_checkpoints_do_not_drift_from_a_month_end_start() {
        // Regression: `date.day() == start.day()` never matched Feb/Apr/Jun...
        let cps = RebalanceFrequency::Monthly.checkpoints(d("2024-01-31"), d("2024-06-30"));
        assert_eq!(
            cps,
            vec![d("2024-02-29"), d("2024-03-31"), d("2024-04-30"), d("2024-05-31"), d("2024-06-30")]
        );
    }

    #[test]
    fn quarterly_and_annual_checkpoints() {
        let q = RebalanceFrequency::Quarterly.checkpoints(d("2024-01-15"), d("2025-01-15"));
        assert_eq!(q, vec![d("2024-04-15"), d("2024-07-15"), d("2024-10-15"), d("2025-01-15")]);
        let a = RebalanceFrequency::Annual.checkpoints(d("2024-02-29"), d("2027-03-01"));
        assert_eq!(a, vec![d("2025-02-28"), d("2026-02-28"), d("2027-02-28")]);
    }

    #[test]
    fn weekly_and_daily_checkpoints() {
        let w = RebalanceFrequency::Weekly.checkpoints(d("2024-01-01"), d("2024-01-22"));
        assert_eq!(w, vec![d("2024-01-08"), d("2024-01-15"), d("2024-01-22")]);
        let dly = RebalanceFrequency::Daily.checkpoints(d("2024-01-01"), d("2024-01-03"));
        assert_eq!(dly, vec![d("2024-01-02"), d("2024-01-03")]);
    }

    #[test]
    fn checkpoints_empty_when_window_is_shorter_than_the_period() {
        assert!(RebalanceFrequency::Annual.checkpoints(d("2024-01-01"), d("2024-06-01")).is_empty());
    }

    #[test]
    fn buy_and_hold_returns_use_first_and_last_price() {
        let mut m: HashMap<String, HashMap<NaiveDate, f64>> = HashMap::new();
        m.insert("A".into(), [(d("2024-01-02"), 100.0), (d("2024-01-05"), 150.0), (d("2024-01-03"), 90.0)].into_iter().collect());
        m.insert("BAD".into(), [(d("2024-01-02"), 0.0), (d("2024-01-05"), 5.0)].into_iter().collect());
        let r = universe_buy_and_hold_returns(&m);
        assert!((r["A"] - 0.5).abs() < 1e-12);
        assert!(!r.contains_key("BAD"), "zero start price must be excluded");
    }
}
