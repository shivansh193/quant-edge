//! Point-in-time S&P 500 membership.
//!
//! Today's universe is (correctly) today's S&P 500. Scoring a *past* date with
//! today's constituent list is survivorship bias: every name that was removed
//! (bankruptcy, acquisition, demotion) is invisible, which can only flatter a
//! backtest. This module answers "who was actually in the index on date D"
//! from a community-maintained dataset (fja05680/sp500 on GitHub): one row per
//! change event, `date,tickers`, covering 1996-present. Verified directly
//! against a known fact: TSLA is absent from the 2020-12-01 snapshot and
//! present from 2021-01-01 (it joined 2020-12-21).
//!
//! This fixes *which tickers count as in-universe* for a historical date. It
//! does not fix data availability for long-delisted names Yahoo no longer
//! serves at all — those are still skipped (same as any unresolvable ticker
//! today), so some historical churn remains unaddressable without a paid
//! data vendor. See docs/METHODOLOGY.md.

use anyhow::{Context, Result};
use chrono::NaiveDate;
use std::collections::HashSet;
use tracing::info;

use crate::data::cache::Cache;

const SOURCE_URL: &str = "https://raw.githubusercontent.com/fja05680/sp500/master/S%26P%20500%20Historical%20Components%20%26%20Changes%20(Updated).csv";
/// The dataset is updated occasionally, not daily; a week-old copy is fine.
const CACHE_TTL_DAYS: i64 = 7;

#[derive(Debug, Clone)]
pub struct MembershipSnapshot {
    pub date: NaiveDate,
    pub tickers: Vec<String>,
}

/// Parse the `date,tickers` CSV (tickers is a quoted, comma-separated list).
/// Malformed rows are skipped rather than failing the whole parse.
pub fn parse_csv(text: &str) -> Vec<MembershipSnapshot> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        // Format: YYYY-MM-DD,"TICK1,TICK2,...". The date has no commas, so
        // splitting once on the first comma is safe even though the ticker
        // list itself contains commas.
        let Some((date_str, rest)) = line.split_once(',') else { continue };
        let Ok(date) = date_str.trim().parse::<NaiveDate>() else { continue };
        let tickers: Vec<String> = rest
            .trim()
            .trim_matches('"')
            .split(',')
            .map(|t| t.trim().to_uppercase())
            .filter(|t| !t.is_empty())
            .collect();
        if !tickers.is_empty() {
            out.push(MembershipSnapshot { date, tickers });
        }
    }
    out.sort_by_key(|s| s.date);
    out
}

pub struct HistoricalMembership {
    cache: Cache,
}

impl HistoricalMembership {
    pub fn new(cache: Cache) -> Self {
        Self { cache }
    }

    /// Make sure the dataset is cached (downloads at most weekly).
    pub async fn ensure_cached(&self) -> Result<()> {
        if self.cache.has_sp500_membership_cache(CACHE_TTL_DAYS) {
            return Ok(());
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .user_agent("portfolio-sim/0.1 (point-in-time S&P 500 membership)")
            .build()
            .context("client build failed")?;
        let text = client
            .get(SOURCE_URL)
            .send()
            .await
            .context("S&P 500 historical membership fetch failed")?
            .error_for_status()
            .context("S&P 500 historical membership returned an error status")?
            .text()
            .await
            .context("S&P 500 historical membership body read failed")?;

        let snapshots = parse_csv(&text);
        anyhow::ensure!(snapshots.len() > 100, "parsed suspiciously few snapshots ({})", snapshots.len());
        self.cache.save_sp500_membership(&snapshots)?;
        info!(n = snapshots.len(), first = %snapshots[0].date, last = %snapshots[snapshots.len()-1].date, "S&P 500 point-in-time membership cached");
        Ok(())
    }

    /// Membership on the latest change-date on or before `as_of`. Dates
    /// before the dataset's earliest snapshot fall back to that earliest
    /// snapshot (1996) rather than returning nothing.
    pub async fn members_as_of(&self, as_of: NaiveDate) -> Result<Vec<String>> {
        self.ensure_cached().await?;
        self.cache.sp500_membership_as_of(as_of)
    }

    /// Union of every distinct ticker that was a member at any point in
    /// `[from, to]`. Used to resolve one superset universe up front instead
    /// of re-resolving per day; for a short window this is barely larger
    /// than any single day's membership, since the index rarely changes.
    pub async fn union_over(&self, from: NaiveDate, to: NaiveDate) -> Result<HashSet<String>> {
        self.ensure_cached().await?;
        self.cache.sp500_membership_union(from, to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn parses_the_real_csv_header_and_quoted_ticker_list() {
        let text = "date,tickers\n2020-01-02,\"AAPL,MSFT,ZTS\"\n2021-06-01,\"AAPL,GME,ZTS\"\n";
        let snaps = parse_csv(text);
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].date, d("2020-01-02"));
        assert_eq!(snaps[0].tickers, vec!["AAPL", "MSFT", "ZTS"]);
        assert_eq!(snaps[1].tickers, vec!["AAPL", "GME", "ZTS"]);
    }

    #[test]
    fn malformed_rows_are_skipped_not_fatal() {
        let text = "date,tickers\nnot-a-date,\"AAPL\"\n2020-01-02,\"AAPL,MSFT\"\n,\n";
        let snaps = parse_csv(text);
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].date, d("2020-01-02"));
    }

    #[test]
    fn output_is_sorted_by_date_regardless_of_input_order() {
        let text = "date,tickers\n2022-01-01,\"B\"\n2020-01-01,\"A\"\n2021-01-01,\"C\"\n";
        let snaps = parse_csv(text);
        let dates: Vec<NaiveDate> = snaps.iter().map(|s| s.date).collect();
        assert_eq!(dates, vec![d("2020-01-01"), d("2021-01-01"), d("2022-01-01")]);
    }
}
