//! Data-staleness detection: is the price feed for a ticker actually
//! keeping up, or silently stuck?
//!
//! A ticker with no fresh bars usually means one of: Yahoo doesn't know the
//! symbol anymore (delisted, renamed, wrong suffix), the exchange has been
//! closed for an unusually long stretch, or the fetch has been silently
//! failing and falling back to cache every time. All three are worth a human
//! noticing, and none of them currently surface anywhere.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::data::cache::Cache;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleTicker {
    pub ticker: String,
    /// The most recent price bar on file, if any.
    pub last_bar_date: Option<NaiveDate>,
    /// Trading days' worth of calendar time since that bar (or since
    /// forever, if there's no bar at all — reported as `i64::MAX`).
    pub days_stale: i64,
}

/// Tickers whose latest cached price bar is more than `max_age_days`
/// calendar days before `as_of` (or missing entirely). `max_age_days`
/// should allow for weekends/holidays — 5 is a reasonable default for a
/// daily job checking yesterday's close.
pub fn find_stale(cache: &Cache, tickers: &[String], as_of: NaiveDate, max_age_days: i64) -> Vec<StaleTicker> {
    let mut out: Vec<StaleTicker> = tickers
        .iter()
        .filter_map(|ticker| {
            let last = cache
                .get_price_bars(ticker, as_of - chrono::Duration::days(30), as_of)
                .ok()
                .and_then(|bars| bars.into_iter().max_by_key(|b| b.date))
                .map(|b| b.date);
            let days_stale = match last {
                Some(d) => (as_of - d).num_days(),
                None => i64::MAX,
            };
            (days_stale > max_age_days).then_some(StaleTicker { ticker: ticker.clone(), last_bar_date: last, days_stale })
        })
        .collect();
    out.sort_by(|a, b| b.days_stale.cmp(&a.days_stale).then_with(|| a.ticker.cmp(&b.ticker)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::source::PriceBar;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn bar(date: &str) -> PriceBar {
        PriceBar { date: d(date), open: 1.0, high: 1.0, low: 1.0, close: 1.0, adj_close: 1.0, volume: 1 }
    }

    #[test]
    fn flags_a_ticker_whose_data_stopped_updating() {
        let cache = Cache::open(":memory:").unwrap();
        cache.insert_price_bars("FRESH", &[bar("2024-06-10"), bar("2024-06-14")]).unwrap();
        cache.insert_price_bars("STALE", &[bar("2024-05-01"), bar("2024-05-02")]).unwrap();
        // NEVER has no bars at all.

        let stale = find_stale(&cache, &["FRESH".into(), "STALE".into(), "NEVER".into()], d("2024-06-14"), 5);
        let tickers: Vec<&str> = stale.iter().map(|s| s.ticker.as_str()).collect();
        assert!(!tickers.contains(&"FRESH"), "fresh as of today must not be flagged");
        assert!(tickers.contains(&"STALE"));
        assert!(tickers.contains(&"NEVER"));
    }

    #[test]
    fn worst_offenders_sort_first() {
        let cache = Cache::open(":memory:").unwrap();
        cache.insert_price_bars("A", &[bar("2024-06-01")]).unwrap(); // 13 days stale
        cache.insert_price_bars("B", &[bar("2024-05-01")]).unwrap(); // 44 days stale
        let stale = find_stale(&cache, &["A".into(), "B".into()], d("2024-06-14"), 5);
        assert_eq!(stale[0].ticker, "B", "the more-stale ticker sorts first");
        assert!(stale[0].days_stale > stale[1].days_stale);
    }

    #[test]
    fn a_missing_ticker_reports_no_last_bar_date() {
        let cache = Cache::open(":memory:").unwrap();
        let stale = find_stale(&cache, &["GHOST".into()], d("2024-06-14"), 5);
        assert_eq!(stale.len(), 1);
        assert!(stale[0].last_bar_date.is_none());
        assert_eq!(stale[0].days_stale, i64::MAX);
    }

    #[test]
    fn a_generous_threshold_hides_ordinary_weekend_gaps() {
        let cache = Cache::open(":memory:").unwrap();
        cache.insert_price_bars("X", &[bar("2024-06-14")]).unwrap(); // Friday
        // Checking as of the following Monday: 3 calendar days, well within a 5-day threshold.
        let stale = find_stale(&cache, &["X".into()], d("2024-06-17"), 5);
        assert!(stale.is_empty());
    }
}
