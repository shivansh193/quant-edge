use anyhow::{Context, Result};
use chrono::{Datelike, Duration, NaiveDate};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::info;

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
    /// Returns true if `date` is a rebalance checkpoint given this frequency.
    pub fn is_checkpoint(&self, date: NaiveDate, start: NaiveDate) -> bool {
        match self {
            RebalanceFrequency::Daily     => true,
            RebalanceFrequency::Weekly    => date.weekday() == start.weekday(),
            RebalanceFrequency::Monthly   => date.day() == start.day(),
            RebalanceFrequency::Quarterly => {
                date.day() == start.day()
                    && (date.month() == start.month()
                        || date.month() == (start.month() + 2) % 12 + 1
                        || date.month() == (start.month() + 5) % 12 + 1
                        || date.month() == (start.month() + 8) % 12 + 1)
            }
            RebalanceFrequency::Annual    => {
                date.day() == start.day() && date.month() == start.month()
            }
        }
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

        let benchmark_map: HashMap<NaiveDate, f64> = benchmark_bars
            .iter()
            .map(|b| (b.date, b.adj_close))
            .collect();

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

        let mut holdings: HashMap<String, f64> =
            self.allocate(&current_weights, cfg.initial_capital, &price_data, cfg.start_date);

        // ── Tracking structures ───────────────────────────────────────────────

        let mut snapshots:       Vec<PortfolioSnapshot>            = Vec::new();
        let mut role_swap_count: HashMap<Role, usize>              = HashMap::new();
        let mut role_weight_sum: HashMap<Role, f64>                = HashMap::new();
        let mut role_return_sum: HashMap<Role, f64>                = HashMap::new();
        let mut industry_returns: HashMap<u32, (String, f64, f64)> = HashMap::new();
        // industry_code → (name, start_value, current_value)

        let benchmark_start = benchmark_map
            .get(&cfg.start_date)
            .copied()
            .unwrap_or(1.0);

        // ── Day loop ──────────────────────────────────────────────────────────

        let mut date = cfg.start_date;

        while date <= cfg.end_date {
            // Skip weekends — markets closed
            if is_weekend(date) {
                date = date.succ_opt().unwrap_or(date);
                continue;
            }

            // Rebalance if this is a checkpoint (and not the very first day)
            if date > cfg.start_date && cfg.rebalance_freq.is_checkpoint(date, cfg.start_date) {
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

                // Re-allocate holdings at new weights using current portfolio value
                let portfolio_value =
                    self.compute_portfolio_value(&holdings, &price_data, date);
                holdings = self.allocate(
                    &current_weights,
                    portfolio_value,
                    &price_data,
                    date,
                );
            }

            // ── Compute portfolio value for today ─────────────────────────────

            let portfolio_value =
                self.compute_portfolio_value(&holdings, &price_data, date);

            let benchmark_price = benchmark_map.get(&date).copied().unwrap_or_else(|| {
                // Forward-fill: find most recent benchmark price
                self.last_known_price(&benchmark_bars, date)
            });

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

        Ok(SimulationResult {
            snapshots,
            swap_log: all_swaps,
            role_performance,
            industry_perf,
            config: self.config.clone(),
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

    fn last_known_price(&self, bars: &[crate::data::source::PriceBar], date: NaiveDate) -> f64 {
        bars.iter()
            .filter(|b| b.date <= date)
            .last()
            .map(|b| b.adj_close)
            .unwrap_or(1.0)
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

// ── Calendar helpers ──────────────────────────────────────────────────────────

fn is_weekend(date: NaiveDate) -> bool {
    use chrono::Weekday;
    matches!(date.weekday(), Weekday::Sat | Weekday::Sun)
}