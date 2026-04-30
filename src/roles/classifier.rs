use anyhow::Result;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{info, warn};
use chrono::Datelike;

use crate::data::source::{DataSource, FundamentalSnapshot, PriceBar};
use crate::universe::builder::{CompanySlot, Universe};

// ── Role definitions ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    FastestGrower,    // highest revenue CAGR 3yr
    LargestByRevenue, // highest TTM revenue
    MostProfitable,   // highest net margin %
    MostLeveraged,    // highest debt/equity
    ConsumerReach,    // highest market share proxy
    DeepValue,        // lowest P/B ratio
    MomentumLeader,   // highest 12m-1m price return
}

impl Role {
    pub fn all() -> Vec<Role> {
        vec![
            Role::FastestGrower,
            Role::LargestByRevenue,
            Role::MostProfitable,
            Role::MostLeveraged,
            Role::ConsumerReach,
            Role::DeepValue,
            Role::MomentumLeader,
        ]
    }

    pub fn label(&self) -> &'static str {
        match self {
            Role::FastestGrower    => "FastestGrower",
            Role::LargestByRevenue => "LargestByRevenue",
            Role::MostProfitable   => "MostProfitable",
            Role::MostLeveraged    => "MostLeveraged",
            Role::ConsumerReach    => "ConsumerReach",
            Role::DeepValue        => "DeepValue",
            Role::MomentumLeader   => "MomentumLeader",
        }
    }

    /// Which fundamental field drives this role's ranking.
    /// Returns None for roles computed from price history (Momentum).
    pub fn fundamental_field(&self) -> Option<&'static str> {
        match self {
            Role::FastestGrower    => Some("revenue_cagr_3yr"),
            Role::LargestByRevenue => Some("revenue_ttm"),
            Role::MostProfitable   => Some("net_margin_pct"),
            Role::MostLeveraged    => Some("debt_to_equity"),
            Role::ConsumerReach    => Some("market_share_proxy"),
            Role::DeepValue        => Some("price_to_book"),
            Role::MomentumLeader   => None, // computed from price_history
        }
    }

    /// Higher is better for most roles — DeepValue is the exception (lowest P/B wins).
    pub fn higher_is_better(&self) -> bool {
        !matches!(self, Role::DeepValue)
    }
}

// ── Output types ──────────────────────────────────────────────────────────────

/// One company assigned to one role in one industry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleAssignment {
    pub industry_code: u32,
    pub industry_name: String,
    pub role:          Role,
    pub ticker:        String,
    pub company_name:  String,
    pub metric_value:  f64,   // the score that won this role
    pub as_of:         NaiveDate,
}

/// All role assignments for one industry at one point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndustryRoster {
    pub industry_code: u32,
    pub industry_name: String,
    pub as_of:         NaiveDate,
    /// role → winning assignment (may be missing if no eligible company)
    pub assignments:   HashMap<Role, RoleAssignment>,
}

impl IndustryRoster {
    /// All tickers currently holding a role in this industry.
    pub fn active_tickers(&self) -> Vec<&str> {
        self.assignments
            .values()
            .map(|a| a.ticker.as_str())
            .collect()
    }

    /// Check if a ticker holds any role in this roster.
    pub fn holds_any_role(&self, ticker: &str) -> bool {
        self.assignments.values().any(|a| a.ticker == ticker)
    }
}

// ── Scored candidate (internal) ───────────────────────────────────────────────

#[derive(Debug)]
struct Candidate {
    ticker:       String,
    company_name: String,
    score:        f64,
}

// ── Classifier ────────────────────────────────────────────────────────────────

pub struct RoleClassifier<'a> {
    source:         &'a dyn DataSource,
    active_roles:   Vec<Role>,
}

impl<'a> RoleClassifier<'a> {
    pub fn new(source: &'a dyn DataSource, active_roles: Vec<Role>) -> Self {
        Self { source, active_roles }
    }

    /// Classify all industries in the universe as of `date`.
    /// Returns one IndustryRoster per industry that has at least one assignment.
    pub async fn classify_universe(
        &self,
        universe: &Universe,
        date: NaiveDate,
    ) -> Result<HashMap<u32, IndustryRoster>> {
        use futures::future::join_all;

        let futures: Vec<_> = universe
            .by_industry
            .iter()
            .map(|(&code, slots)| self.classify_industry(code, slots, date))
            .collect();

        let results = join_all(futures).await;

        let mut rosters = HashMap::new();
        for result in results {
            match result {
                Ok(roster) => {
                    rosters.insert(roster.industry_code, roster);
                }
                Err(e) => {
                    warn!("Industry classification failed: {:#}", e);
                }
            }
        }

        info!(
            "Classified {} industries as of {}",
            rosters.len(),
            date
        );

        Ok(rosters)
    }

    /// Classify one industry — fetch fundamentals + price data for all
    /// candidates, score each role, pick winners.
    async fn classify_industry(
        &self,
        industry_code: u32,
        slots: &[CompanySlot],
        date: NaiveDate,
    ) -> Result<IndustryRoster> {
        if slots.is_empty() {
            return Ok(IndustryRoster {
                industry_code,
                industry_name: String::new(),
                as_of: date,
                assignments: HashMap::new(),
            });
        }

        let industry_name = slots[0].industry_name.clone();

        // Fetch fundamentals for all candidates concurrently
        let fund_futures: Vec<_> = slots
            .iter()
            .map(|s| self.source.fundamentals(&s.ticker, date))
            .collect();

        let fund_results = futures::future::join_all(fund_futures).await;

        // Build a map of ticker → fundamentals (drop failures)
        let mut fundamentals: HashMap<String, FundamentalSnapshot> = HashMap::new();
        for (slot, result) in slots.iter().zip(fund_results) {
            match result {
                Ok(snap) => { fundamentals.insert(slot.ticker.clone(), snap); }
                Err(e) => {
                    warn!(
                        ticker = %slot.ticker,
                        industry = %industry_name,
                        "Fundamentals fetch failed: {:#}", e
                    );
                }
            }
        }

        // Compute market_share_proxy for ConsumerReach:
        // each company's share = its revenue_ttm / sum of all revenue_ttm in industry
        let total_revenue: f64 = fundamentals
            .values()
            .filter_map(|f| f.revenue_ttm)
            .sum();

        // Compute momentum for all tickers (price-derived, not from fundamentals)
        let momentum = if self.active_roles.contains(&Role::MomentumLeader) {
            self.compute_momentum_batch(slots, date).await
        } else {
            HashMap::new()
        };

        // Score each role
        let mut assignments: HashMap<Role, RoleAssignment> = HashMap::new();

        for role in &self.active_roles {
            let candidates = self.score_candidates(
                slots,
                &fundamentals,
                &momentum,
                total_revenue,
                role,
            );

            if candidates.is_empty() {
                warn!(
                    role = %role.label(),
                    industry = %industry_name,
                    "No eligible candidates"
                );
                continue;
            }

            // Pick the winner
            let winner = &candidates[0];

            assignments.insert(
                role.clone(),
                RoleAssignment {
                    industry_code,
                    industry_name: industry_name.clone(),
                    role: role.clone(),
                    ticker: winner.ticker.clone(),
                    company_name: winner.company_name.clone(),
                    metric_value: winner.score,
                    as_of: date,
                },
            );
        }

        Ok(IndustryRoster {
            industry_code,
            industry_name,
            as_of: date,
            assignments,
        })
    }

    /// Score all candidates for a given role.
    /// Returns sorted list (best first), filtered to those with valid data.
    fn score_candidates(
        &self,
        slots: &[CompanySlot],
        fundamentals: &HashMap<String, FundamentalSnapshot>,
        momentum: &HashMap<String, f64>,
        total_revenue: f64,
        role: &Role,
    ) -> Vec<Candidate> {
        let mut candidates: Vec<Candidate> = slots
            .iter()
            .filter_map(|slot| {
                let score = self.score_for_role(
                    &slot.ticker,
                    fundamentals,
                    momentum,
                    total_revenue,
                    role,
                )?;

                Some(Candidate {
                    ticker:       slot.ticker.clone(),
                    company_name: slot.name.clone(),
                    score,
                })
            })
            .collect();

        // Sort: higher is better for most roles, lower for DeepValue
        if role.higher_is_better() {
            candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        } else {
            candidates.sort_by(|a, b| a.score.partial_cmp(&b.score).unwrap());
        }

        candidates
    }

    /// Extract the numeric score for a single ticker + role combination.
    /// Returns None if the required metric is unavailable → candidate is skipped.
    fn score_for_role(
        &self,
        ticker: &str,
        fundamentals: &HashMap<String, FundamentalSnapshot>,
        momentum: &HashMap<String, f64>,
        total_revenue: f64,
        role: &Role,
    ) -> Option<f64> {
        match role {
            Role::MomentumLeader => momentum.get(ticker).copied(),

            Role::ConsumerReach => {
                // market_share_proxy = revenue_ttm / industry_total_revenue
                let rev = fundamentals.get(ticker)?.revenue_ttm?;
                if total_revenue > 0.0 {
                    Some(rev / total_revenue)
                } else {
                    None
                }
            }

            Role::FastestGrower => {
                // revenue_cagr_3yr is None from Yahoo — computed if we have
                // 3 years of revenue snapshots in cache; otherwise skip
                fundamentals.get(ticker)?.revenue_cagr_3yr
            }

            Role::LargestByRevenue => fundamentals.get(ticker)?.revenue_ttm,
            Role::MostProfitable   => fundamentals.get(ticker)?.net_margin_pct,
            Role::MostLeveraged    => fundamentals.get(ticker)?.debt_to_equity,
            Role::DeepValue        => fundamentals.get(ticker)?.price_to_book,
        }
    }

    // ── Momentum computation ──────────────────────────────────────────────────

    /// 12m-1m momentum: return from (date - 12 months) to (date - 1 month).
    /// Skips the most recent month to avoid short-term reversal noise.
    async fn compute_momentum_batch(
        &self,
        slots: &[CompanySlot],
        date: NaiveDate,
    ) -> HashMap<String, f64> {
        use futures::future::join_all;

        let start = subtract_months(date, 12);
        let end   = subtract_months(date, 1);

        let futures: Vec<_> = slots
            .iter()
            .map(|s| self.source.price_history(&s.ticker, start, end))
            .collect();

        let results = join_all(futures).await;

        let mut momentum = HashMap::new();

        for (slot, result) in slots.iter().zip(results) {
            match result {
                Ok(bars) => {
                    if let Some(m) = compute_price_return(&bars) {
                        momentum.insert(slot.ticker.clone(), m);
                    }
                }
                Err(e) => {
                    warn!(
                        ticker = %slot.ticker,
                        "Momentum price fetch failed: {:#}", e
                    );
                }
            }
        }

        momentum
    }

    // ── Revenue CAGR computation ──────────────────────────────────────────────

    /// Compute 3yr revenue CAGR from cached fundamentals snapshots.
    /// Called by the simulation engine after fundamentals are populated —
    /// patches the FundamentalSnapshot in-place via the cache.
    pub async fn compute_revenue_cagr(
        &self,
        ticker: &str,
        as_of: NaiveDate,
    ) -> Option<f64> {
        let date_3yr_ago = subtract_years(as_of, 3);

        let snap_now  = self.source.fundamentals(ticker, as_of).await.ok()?;
        let snap_past = self.source.fundamentals(ticker, date_3yr_ago).await.ok()?;

        let rev_now  = snap_now.revenue_ttm?;
        let rev_past = snap_past.revenue_ttm?;

        if rev_past <= 0.0 || rev_now <= 0.0 {
            return None;
        }

        // CAGR = (end / start)^(1/n) - 1
        let cagr = (rev_now / rev_past).powf(1.0 / 3.0) - 1.0;
        Some(cagr)
    }

    // ── Swap detection ────────────────────────────────────────────────────────

    /// Compare two rosters for the same industry.
    /// Returns (role, old_ticker, new_ticker) for every role that changed hands.
    pub fn detect_swaps(
        previous: &IndustryRoster,
        current: &IndustryRoster,
    ) -> Vec<(Role, String, String)> {
        let mut swaps = Vec::new();

        for (role, current_assignment) in &current.assignments {
            if let Some(prev_assignment) = previous.assignments.get(role) {
                if prev_assignment.ticker != current_assignment.ticker {
                    swaps.push((
                        role.clone(),
                        prev_assignment.ticker.clone(),
                        current_assignment.ticker.clone(),
                    ));
                }
            }
        }

        swaps
    }
}

// ── Price return helper ───────────────────────────────────────────────────────

/// Simple price return: (last_close - first_close) / first_close
fn compute_price_return(bars: &[PriceBar]) -> Option<f64> {
    let first = bars.first()?.adj_close;
    let last  = bars.last()?.adj_close;
    if first <= 0.0 {
        return None;
    }
    Some((last - first) / first)
}

// ── Date arithmetic helpers ───────────────────────────────────────────────────

fn subtract_months(date: NaiveDate, months: u32) -> NaiveDate {
    let total_months = date.month() as i32 - months as i32;
    let year_offset  = if total_months <= 0 { (total_months - 1) / 12 } else { 0 };
    let new_month    = ((total_months - 1).rem_euclid(12) + 1) as u32;
    let new_year     = date.year() + year_offset;

    // Clamp day to valid range for the target month
    let max_day = days_in_month(new_year, new_month);
    NaiveDate::from_ymd_opt(new_year, new_month, date.day().min(max_day))
        .unwrap_or(date)
}

fn subtract_years(date: NaiveDate, years: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(date.year() - years as i32, date.month(), date.day())
        .unwrap_or(date)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11              => 30,
        2 => if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) { 29 } else { 28 },
        _ => 30,
    }
}