use anyhow::{Context, Result};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{info, warn};

use crate::data::cache::Cache;
use crate::daily::{holding_period_days, min_score_threshold, score_based_picks};
use crate::llm::StrategySpec;
use crate::signals::PickingEngine;
use crate::universe::builder::Universe;

// ── Core types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub ticker:         String,
    pub shares:         f64,
    pub entry_price:    f64,
    pub entry_date:     NaiveDate,
    pub current_price:  f64,
    pub unrealised_pnl: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperPortfolio {
    pub positions:          HashMap<String, Position>,
    pub cash:               f64,
    pub strategy_spec:      StrategySpec,
    pub inception_date:     NaiveDate,
    pub last_rebalance_date: NaiveDate,
}

impl PaperPortfolio {
    /// Total mark-to-market value (cash + positions at current_price).
    pub fn total_value(&self) -> f64 {
        let pos_value: f64 = self
            .positions
            .values()
            .map(|p| p.shares * p.current_price)
            .sum();
        self.cash + pos_value
    }

    /// Number of days since last rebalance.
    pub fn days_since_rebalance(&self, today: NaiveDate) -> i64 {
        (today - self.last_rebalance_date).num_days()
    }

    /// Whether a rebalance is due based on holding_period_days.
    pub fn rebalance_due(&self, today: NaiveDate) -> bool {
        self.days_since_rebalance(today) >= self.strategy_spec.holding_period_days as i64
    }
}

// ── Engine ────────────────────────────────────────────────────────────────────

pub struct PaperTradingEngine {
    cache: Cache,
}

impl PaperTradingEngine {
    pub fn new(cache: Cache) -> Self {
        Self { cache }
    }

    /// Initialise a brand-new paper portfolio from a strategy spec.
    /// Runs the picking engine immediately to populate initial positions.
    pub async fn init_portfolio(
        &self,
        spec: StrategySpec,
        universe: &Universe,
        today: NaiveDate,
        initial_capital: f64,
    ) -> Result<PaperPortfolio> {
        info!("Initialising paper portfolio '{}' with ${:.0}", spec.name, initial_capital);

        let mut picking_engine = PickingEngine::new(self.cache.clone());
        picking_engine.apply_strategy_spec(&spec);

        let scores = picking_engine
            .rank_universe(universe, today)
            .await
            .context("Picking engine failed during paper init")?;

        let top_n = spec.top_n;
        let mut picks: Vec<_> = scores
            .iter()
            .filter(|s| {
                spec.filters.min_score.map(|m| s.composite >= m).unwrap_or(true)
            })
            .take(top_n)
            .collect();

        let alloc = if picks.is_empty() {
            0.0
        } else {
            initial_capital / picks.len() as f64
        };

        let mut positions: HashMap<String, Position> = HashMap::new();
        let mut cash = initial_capital;

        for score in &picks {
            let price = self.fetch_last_price(&score.ticker, today).await;
            if price <= 0.0 {
                warn!("No price for {} — skipping paper position", score.ticker);
                cash += alloc; // keep as cash if we can't price
                continue;
            }
            let shares = alloc / price;
            cash -= shares * price;
            positions.insert(
                score.ticker.clone(),
                Position {
                    ticker:         score.ticker.clone(),
                    shares,
                    entry_price:    price,
                    entry_date:     today,
                    current_price:  price,
                    unrealised_pnl: 0.0,
                },
            );
        }

        let portfolio = PaperPortfolio {
            positions,
            cash,
            strategy_spec: spec,
            inception_date: today,
            last_rebalance_date: today,
        };

        self.save_portfolio(&portfolio)?;
        info!(
            "Paper portfolio initialised: {} positions, ${:.0} cash, ${:.0} total",
            portfolio.positions.len(),
            portfolio.cash,
            portfolio.total_value(),
        );

        Ok(portfolio)
    }

    /// Update or auto-initialise the paper portfolio.
    /// On first call (no portfolio exists): creates one with default settings using
    /// the provided universe and initial_capital.
    /// On subsequent calls: marks to market and rebalances if due.
    pub async fn update_or_init_portfolio(
        &self,
        universe: &Universe,
        today: NaiveDate,
        initial_capital: f64,
    ) -> Result<PaperPortfolio> {
        if self.load_portfolio()?.is_none() {
            info!("No paper portfolio found — auto-initialising with default strategy.");
            let mut spec = StrategySpec::default();
            spec.name = "Daily Engine".to_string();
            spec.holding_period_days = holding_period_days();
            spec.top_n = 15;
            return self.init_portfolio(spec, universe, today, initial_capital).await;
        }
        self.update_portfolio(universe, today).await
    }

    /// Mark existing positions to market, rebalance if due.
    pub async fn update_portfolio(
        &self,
        universe: &Universe,
        today: NaiveDate,
    ) -> Result<PaperPortfolio> {
        let mut portfolio = self
            .load_portfolio()?
            .context("No paper portfolio found — run --paper-update first (auto-initialises)")?;

        // Mark to market
        let tickers: Vec<String> = portfolio.positions.keys().cloned().collect();
        for ticker in &tickers {
            let price = self.fetch_last_price(ticker, today).await;
            if let Some(pos) = portfolio.positions.get_mut(ticker) {
                if price > 0.0 {
                    pos.current_price = price;
                    pos.unrealised_pnl = (price - pos.entry_price) / pos.entry_price * 100.0;
                }
            }
        }

        info!(
            "Marked to market: total value ${:.0}  (rebalance due: {})",
            portfolio.total_value(),
            portfolio.rebalance_due(today),
        );

        // Rebalance if due
        if portfolio.rebalance_due(today) {
            info!("Rebalancing paper portfolio...");

            let threshold = min_score_threshold();
            let mut picking_engine = PickingEngine::new(self.cache.clone());
            picking_engine.apply_strategy_spec(&portfolio.strategy_spec);

            let scores = picking_engine
                .rank_universe(universe, today)
                .await
                .context("Picking engine failed during paper rebalance")?;

            // Use score-based dynamic top-N
            let top_picks = score_based_picks(&scores, threshold);

            let new_tickers: std::collections::HashSet<String> =
                top_picks.iter().map(|s| s.ticker.clone()).collect();
            let held: Vec<String> = portfolio.positions.keys().cloned().collect();

            // Classify: sell / keep / buy
            let mut sold:  Vec<String> = Vec::new();
            let mut kept:  Vec<String> = Vec::new();
            let mut bought: Vec<String> = Vec::new();

            // Sell positions not in new picks
            let portfolio_value = portfolio.total_value();
            for ticker in &held {
                if new_tickers.contains(ticker) {
                    kept.push(ticker.clone());
                } else if let Some(pos) = portfolio.positions.remove(ticker) {
                    let proceeds = pos.shares * pos.current_price;
                    portfolio.cash += proceeds;
                    sold.push(format!(
                        "{} @ {:.2} ({:.0} shares, P&L {:>+.2}%)",
                        ticker, pos.current_price, pos.shares, pos.unrealised_pnl
                    ));
                }
            }

            // Equal-weight allocation across all final positions
            let n_total = top_picks.len().max(1);
            let alloc = portfolio_value / n_total as f64;

            for score in &top_picks {
                if portfolio.positions.contains_key(&score.ticker) {
                    continue; // kept
                }
                let price = self.fetch_last_price(&score.ticker, today).await;
                if price <= 0.0 {
                    continue;
                }
                let shares = alloc / price;
                if shares * price <= portfolio.cash + 1.0 {
                    portfolio.cash -= shares * price;
                    portfolio.positions.insert(
                        score.ticker.clone(),
                        Position {
                            ticker:         score.ticker.clone(),
                            shares,
                            entry_price:    price,
                            entry_date:     today,
                            current_price:  price,
                            unrealised_pnl: 0.0,
                        },
                    );
                    bought.push(format!("{} @ {:.2} ({:.0} shares)", score.ticker, price, shares));
                }
            }

            portfolio.last_rebalance_date = today;

            // Print trade log
            println!();
            println!("\x1b[1m\x1b[97m  REBALANCE TRADE LOG — {}\x1b[0m", today);
            println!("  ─────────────────────────────────────────");
            println!("  SOLD  ({}):", sold.len());
            for s in &sold  { println!("    ✗  {}", s); }
            println!("  KEPT  ({}):", kept.len());
            for k in &kept  { println!("    →  {}", k); }
            println!("  BOUGHT ({}):", bought.len());
            for b in &bought { println!("    ✓  {}", b); }
            println!("  Total positions: {}   Portfolio value: ${:.0}", portfolio.positions.len(), portfolio.total_value());
            println!();
        }

        self.save_portfolio(&portfolio)?;
        Ok(portfolio)
    }

    /// Load portfolio state without rebalancing.
    pub fn status(&self) -> Result<PaperPortfolio> {
        self.load_portfolio()?
            .context("No paper portfolio found — run --paper-init first")
    }

    // ── Persistence ───────────────────────────────────────────────────────────

    pub fn save_portfolio(&self, portfolio: &PaperPortfolio) -> Result<()> {
        let json = serde_json::to_string(portfolio).context("Failed to serialize portfolio")?;
        self.cache.save_paper_portfolio(&json)
    }

    pub fn load_portfolio(&self) -> Result<Option<PaperPortfolio>> {
        match self.cache.load_paper_portfolio()? {
            Some(json) => {
                let p: PaperPortfolio =
                    serde_json::from_str(&json).context("Failed to deserialize paper portfolio")?;
                Ok(Some(p))
            }
            None => Ok(None),
        }
    }

    // ── Price fetch ───────────────────────────────────────────────────────────

    async fn fetch_last_price(&self, ticker: &str, as_of: NaiveDate) -> f64 {
        use chrono::Duration;
        let from = as_of - Duration::days(7); // look back up to 7 days for last close
        match self.cache.get_price_bars(ticker, from, as_of) {
            Ok(bars) => bars.last().map(|b| b.adj_close).unwrap_or(0.0),
            Err(_) => 0.0,
        }
    }
}
