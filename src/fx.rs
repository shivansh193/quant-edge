//! Currency conversion for a mixed NSE (₹) / NYSE ($) portfolio.
//!
//! Documented as a known gap earlier in this project (METHODOLOGY.md: "no FX
//! handling... mixed INR/USD universes are treated in percentage terms").
//! It became a concrete bug once `holdings::portfolio_xirr` started summing
//! per-ticker cash values across a real trade history: an INR trade and a
//! USD trade were added together with no conversion, which is meaningless.
//!
//! Scope: USD and INR only (the two currencies this codebase actually
//! trades), point-in-time (the FX rate is looked up on the flow's own date,
//! the same discipline as everything else in this project — a portfolio
//! valued with today's rate for a trade from a year ago would be wrong in
//! the same way an un-dated fundamental snapshot is wrong).

use anyhow::{Context, Result};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::data::cache::Cache;
use crate::data::prices::PriceSeries;
use crate::data::yahoo::YahooFinance;
use crate::data::DataSource;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Currency {
    Usd,
    Inr,
}

impl Currency {
    /// `.NS` tickers trade in INR; everything else in this codebase is USD —
    /// the same convention `market_matches`/`AutoUniverseBuilder` already use.
    pub fn of_ticker(ticker: &str) -> Self {
        if ticker.ends_with(".NS") { Currency::Inr } else { Currency::Usd }
    }
}

/// USD/INR daily rate (INR per 1 USD), fetched like any other price series
/// via Yahoo's `INR=X` and cached the same way as everything else.
pub struct FxRates {
    cache: Cache,
}

impl FxRates {
    pub fn new(cache: Cache) -> Self {
        Self { cache }
    }

    /// INR per 1 USD, forward-filled to the latest rate on or before `date`.
    pub async fn usd_inr_rate(&self, date: NaiveDate) -> Result<f64> {
        let yahoo = YahooFinance::new(self.cache.clone());
        let bars = yahoo
            .price_history(FX_TICKER, date - chrono::Duration::days(10), date)
            .await
            .context("USD/INR rate fetch failed")?;
        let series = PriceSeries::new(bars);
        series
            .on_or_before(date)
            .map(|b| b.close)
            .with_context(|| format!("no USD/INR rate available on or before {date}"))
    }

    /// Convert `amount` in `from` currency to `to`, at the rate on `date`.
    pub async fn convert(&self, amount: f64, from: Currency, to: Currency, date: NaiveDate) -> Result<f64> {
        if from == to {
            return Ok(amount);
        }
        let rate = self.usd_inr_rate(date).await?; // INR per USD
        Ok(match (from, to) {
            (Currency::Inr, Currency::Usd) => amount / rate,
            (Currency::Usd, Currency::Inr) => amount * rate,
            _ => amount,
        })
    }
}

const FX_TICKER: &str = "INR=X";

/// Pure conversion given an already-known rate — split out from `FxRates` so
/// the arithmetic can be tested without a network call.
pub fn convert_at_rate(amount: f64, from: Currency, to: Currency, inr_per_usd: f64) -> f64 {
    if from == to {
        return amount;
    }
    match (from, to) {
        (Currency::Inr, Currency::Usd) => amount / inr_per_usd,
        (Currency::Usd, Currency::Inr) => amount * inr_per_usd,
        _ => amount,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticker_currency_follows_the_ns_suffix_convention() {
        assert_eq!(Currency::of_ticker("RELIANCE.NS"), Currency::Inr);
        assert_eq!(Currency::of_ticker("AAPL"), Currency::Usd);
        assert_eq!(Currency::of_ticker("TCS.NS"), Currency::Inr);
    }

    #[test]
    fn same_currency_conversion_is_a_no_op_even_with_a_nonsense_rate() {
        assert_eq!(convert_at_rate(100.0, Currency::Usd, Currency::Usd, 0.0), 100.0);
        assert_eq!(convert_at_rate(100.0, Currency::Inr, Currency::Inr, -5.0), 100.0);
    }

    #[test]
    fn inr_to_usd_divides_and_usd_to_inr_multiplies() {
        // At 90 INR/USD: ₹9000 = $100.
        assert!((convert_at_rate(9000.0, Currency::Inr, Currency::Usd, 90.0) - 100.0).abs() < 1e-9);
        assert!((convert_at_rate(100.0, Currency::Usd, Currency::Inr, 90.0) - 9000.0).abs() < 1e-9);
    }

    #[test]
    fn round_trip_is_the_identity() {
        let original = 12345.67;
        let to_usd = convert_at_rate(original, Currency::Inr, Currency::Usd, 87.3);
        let back = convert_at_rate(to_usd, Currency::Usd, Currency::Inr, 87.3);
        assert!((back - original).abs() < 1e-6);
    }
}
