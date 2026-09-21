//! Execution-cost and tax models.
//!
//! A backtest that ignores costs answers the wrong question. This module keeps
//! the cost assumptions in one auditable place:
//!
//!   * [`CostModel`] — commission, half-spread, fixed slippage and a
//!     square-root market-impact term that scales with order size vs. ADV.
//!   * [`TaxModel`] — short/long-term capital-gains tax on realised trades.
//!
//! Tax rates are presets that change with legislation. They are **illustrative
//! defaults, not advice** — verify them for your own situation.

use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ── Execution costs ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostModel {
    /// Fixed commission per order, in account currency.
    pub commission_per_trade: f64,
    /// Proportional commission / fees / taxes on notional (e.g. STT, stamp duty).
    pub commission_bps: f64,
    /// Half of the bid-ask spread, paid on every fill.
    pub half_spread_bps: f64,
    /// Flat slippage on top of the spread.
    pub slippage_bps: f64,
    /// `k` in `impact = k * daily_vol * sqrt(order_notional / ADV)`.
    /// Around 0.5–1.0 is a common empirical range; 0 disables impact.
    pub impact_coefficient: f64,
}

impl CostModel {
    /// No costs at all — for tests and idealised upper bounds.
    pub fn zero() -> Self {
        Self {
            commission_per_trade: 0.0,
            commission_bps: 0.0,
            half_spread_bps: 0.0,
            slippage_bps: 0.0,
            impact_coefficient: 0.0,
        }
    }

    /// Conservative default for liquid large-cap equities.
    pub fn default_equity() -> Self {
        Self {
            commission_per_trade: 1.0,
            commission_bps: 0.0,
            half_spread_bps: 2.0,
            slippage_bps: 8.0,
            impact_coefficient: 0.5,
        }
    }

    /// Fractional market impact for an order of `notional` against `adv`
    /// (average daily dollar volume) at daily volatility `daily_vol`.
    pub fn impact_fraction(&self, notional: f64, adv: Option<f64>, daily_vol: Option<f64>) -> f64 {
        match (adv, daily_vol) {
            (Some(adv), Some(vol)) if adv > 0.0 && vol > 0.0 && notional > 0.0 => {
                self.impact_coefficient * vol * (notional / adv).sqrt()
            }
            _ => 0.0,
        }
    }

    /// Price actually paid/received: the reference price moved *against* the
    /// trader by spread + slippage + impact.
    pub fn execution_price(
        &self,
        ref_price: f64,
        is_buy: bool,
        notional: f64,
        adv: Option<f64>,
        daily_vol: Option<f64>,
    ) -> f64 {
        let frac = (self.half_spread_bps + self.slippage_bps) / 10_000.0
            + self.impact_fraction(notional, adv, daily_vol);
        if is_buy {
            ref_price * (1.0 + frac)
        } else {
            ref_price * (1.0 - frac).max(0.0)
        }
    }

    /// Commission for an order of the given notional.
    pub fn commission(&self, notional: f64) -> f64 {
        (self.commission_per_trade + self.commission_bps / 10_000.0 * notional.abs()).max(0.0)
    }
}

impl Default for CostModel {
    fn default() -> Self {
        Self::default_equity()
    }
}

// ── Tax ───────────────────────────────────────────────────────────────────────

/// One realised, closed round trip — the only thing tax needs to know.
#[derive(Debug, Clone, Copy)]
pub struct RealizedGain {
    pub entry_date: NaiveDate,
    pub exit_date: NaiveDate,
    /// Net profit in account currency (after commissions/slippage).
    pub pnl: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaxModel {
    pub short_term_rate: f64,
    pub long_term_rate: f64,
    /// Holding period (days) above which a gain is long-term.
    pub long_term_days: i64,
    /// Long-term gains exempt per tax year, in account currency.
    pub annual_long_term_exemption: f64,
    /// Month (1–12) in which the tax year starts (India: 4, US: 1).
    pub fiscal_year_start_month: u32,
}

impl TaxModel {
    /// No tax.
    pub fn none() -> Self {
        Self {
            short_term_rate: 0.0,
            long_term_rate: 0.0,
            long_term_days: 365,
            annual_long_term_exemption: 0.0,
            fiscal_year_start_month: 1,
        }
    }

    /// India listed equity (post 23-Jul-2024): STCG 20%, LTCG 12.5% above
    /// ₹1.25 lakh per financial year, long-term after 12 months. Excludes
    /// surcharge/cess and STT. **Verify current rates.**
    pub fn india_equity() -> Self {
        Self {
            short_term_rate: 0.20,
            long_term_rate: 0.125,
            long_term_days: 365,
            annual_long_term_exemption: 125_000.0,
            fiscal_year_start_month: 4,
        }
    }

    /// US federal, illustrative: 24% short-term (ordinary income bracket),
    /// 15% long-term. Ignores state tax, NIIT and wash-sale rules.
    pub fn us_taxable() -> Self {
        Self {
            short_term_rate: 0.24,
            long_term_rate: 0.15,
            long_term_days: 365,
            annual_long_term_exemption: 0.0,
            fiscal_year_start_month: 1,
        }
    }

    fn tax_year(&self, d: NaiveDate) -> i32 {
        if self.fiscal_year_start_month <= 1 || d.month() >= self.fiscal_year_start_month {
            d.year()
        } else {
            d.year() - 1
        }
    }

    /// Tax due on `gains`, computed independently per tax year.
    ///
    /// Rules modelled: short-term losses offset short-term then long-term
    /// gains; long-term losses offset only long-term gains; the annual
    /// exemption applies to net long-term gains. **Not modelled:** loss
    /// carry-forward, wash sales, surcharge/cess.
    pub fn assess(&self, gains: &[RealizedGain]) -> TaxReport {
        #[derive(Default)]
        struct Bucket {
            st: f64,
            lt: f64,
        }
        let mut by_year: BTreeMap<i32, Bucket> = BTreeMap::new();
        for g in gains {
            let b = by_year.entry(self.tax_year(g.exit_date)).or_default();
            if (g.exit_date - g.entry_date).num_days() > self.long_term_days {
                b.lt += g.pnl;
            } else {
                b.st += g.pnl;
            }
        }

        let mut years = Vec::new();
        let mut total_tax = 0.0;
        for (year, b) in by_year {
            let mut st = b.st;
            let mut lt = b.lt;
            // A short-term loss can shelter long-term gains.
            if st < 0.0 && lt > 0.0 {
                let used = (-st).min(lt);
                st += used;
                lt -= used;
            }
            let taxable_st = st.max(0.0);
            let taxable_lt = (lt - self.annual_long_term_exemption).max(0.0);
            let tax = taxable_st * self.short_term_rate + taxable_lt * self.long_term_rate;
            total_tax += tax;
            years.push(TaxYear {
                tax_year: year,
                short_term_pnl: b.st,
                long_term_pnl: b.lt,
                tax,
            });
        }
        TaxReport { total_tax, years }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaxYear {
    pub tax_year: i32,
    pub short_term_pnl: f64,
    pub long_term_pnl: f64,
    pub tax: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaxReport {
    pub total_tax: f64,
    pub years: Vec<TaxYear>,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn buys_pay_up_and_sells_receive_less() {
        let c = CostModel { half_spread_bps: 5.0, slippage_bps: 5.0, ..CostModel::zero() };
        let buy = c.execution_price(100.0, true, 1_000.0, None, None);
        let sell = c.execution_price(100.0, false, 1_000.0, None, None);
        assert!((buy - 100.10).abs() < 1e-9);
        assert!((sell - 99.90).abs() < 1e-9);
    }

    #[test]
    fn impact_grows_with_order_size_but_sub_linearly() {
        let c = CostModel { impact_coefficient: 1.0, ..CostModel::zero() };
        let small = c.impact_fraction(10_000.0, Some(1_000_000.0), Some(0.02));
        let large = c.impact_fraction(40_000.0, Some(1_000_000.0), Some(0.02));
        assert!(large > small);
        // 4x the size → 2x the impact (square-root law).
        assert!((large / small - 2.0).abs() < 1e-9);
    }

    #[test]
    fn impact_is_zero_without_liquidity_data() {
        let c = CostModel { impact_coefficient: 1.0, ..CostModel::zero() };
        assert_eq!(c.impact_fraction(10_000.0, None, Some(0.02)), 0.0);
        assert_eq!(c.impact_fraction(10_000.0, Some(0.0), Some(0.02)), 0.0);
    }

    #[test]
    fn commission_combines_fixed_and_proportional() {
        let c = CostModel { commission_per_trade: 1.0, commission_bps: 10.0, ..CostModel::zero() };
        assert!((c.commission(10_000.0) - 11.0).abs() < 1e-9);
    }

    #[test]
    fn short_term_gain_taxed_at_short_rate() {
        let t = TaxModel::india_equity();
        let r = t.assess(&[RealizedGain { entry_date: d("2024-08-01"), exit_date: d("2024-09-01"), pnl: 10_000.0 }]);
        assert!((r.total_tax - 2_000.0).abs() < 1e-9);
    }

    #[test]
    fn long_term_gain_uses_exemption_then_rate() {
        let t = TaxModel::india_equity();
        let r = t.assess(&[RealizedGain { entry_date: d("2023-01-01"), exit_date: d("2024-06-01"), pnl: 225_000.0 }]);
        // (225,000 − 125,000) × 12.5% = 12,500
        assert!((r.total_tax - 12_500.0).abs() < 1e-9);
    }

    #[test]
    fn short_term_loss_shelters_long_term_gain_but_not_vice_versa() {
        let t = TaxModel { annual_long_term_exemption: 0.0, ..TaxModel::india_equity() };
        let r = t.assess(&[
            RealizedGain { entry_date: d("2023-01-01"), exit_date: d("2024-06-01"), pnl: 100_000.0 },
            RealizedGain { entry_date: d("2024-05-01"), exit_date: d("2024-06-15"), pnl: -40_000.0 },
        ]);
        assert!((r.total_tax - 60_000.0 * 0.125).abs() < 1e-9);

        // A long-term loss must NOT reduce short-term tax.
        let r2 = t.assess(&[
            RealizedGain { entry_date: d("2023-01-01"), exit_date: d("2024-06-01"), pnl: -50_000.0 },
            RealizedGain { entry_date: d("2024-05-01"), exit_date: d("2024-06-15"), pnl: 10_000.0 },
        ]);
        assert!((r2.total_tax - 2_000.0).abs() < 1e-9);
    }

    #[test]
    fn net_losses_produce_no_tax_and_no_refund() {
        let r = TaxModel::us_taxable().assess(&[RealizedGain { entry_date: d("2024-01-10"), exit_date: d("2024-02-10"), pnl: -5_000.0 }]);
        assert_eq!(r.total_tax, 0.0);
    }

    #[test]
    fn fiscal_year_boundary_splits_years() {
        let t = TaxModel::india_equity(); // FY starts 1 April
        let r = t.assess(&[
            RealizedGain { entry_date: d("2024-01-01"), exit_date: d("2024-03-31"), pnl: 1_000.0 },
            RealizedGain { entry_date: d("2024-01-01"), exit_date: d("2024-04-01"), pnl: 1_000.0 },
        ]);
        assert_eq!(r.years.len(), 2);
        assert_eq!(r.years[0].tax_year, 2023);
        assert_eq!(r.years[1].tax_year, 2024);
    }
}
