//! A read-only, as-of-dated view of everything we know.
//!
//! Look-ahead bias is the most common way a backtest lies, and it is easy to
//! introduce by accident: one query that forgets `<= as_of`, one cache that was
//! filled later. `AsOf` makes it structural instead of a matter of discipline:
//!
//!   * there is **no method that takes a date later than the view's date** and
//!     no way to reach the underlying cache from a view;
//!   * every query is clamped to `as_of` in SQL *and* the results are filtered
//!     again in Rust, so a bug in either layer cannot leak the future;
//!   * fundamentals for historical dates come only from dated SEC filings —
//!     never from a current snapshot that happens to be labelled with an old
//!     date (the bug this replaces).
//!
//! Network access is a separate concern (the fetchers). They fill the cache;
//! signals read through an `AsOf`.

use anyhow::Result;
use chrono::{Duration, Local, NaiveDate};

use super::cache::Cache;
use super::sec_facts::PitFact;
use super::source::{FundamentalSnapshot, PriceBar};
use super::types::{InsiderTrade, MacroDataPoint, NewsItem, RedditSnapshot};

/// A date within this many days of today counts as "live": current-snapshot
/// sources (Yahoo fundamentals, GDELT, Reddit) describe it accurately.
pub const LIVE_GRACE_DAYS: i64 = 3;

/// True when `as_of` is far enough in the past that *current* snapshots must
/// not be used to describe it.
pub fn is_historical(as_of: NaiveDate) -> bool {
    as_of < Local::now().date_naive() - Duration::days(LIVE_GRACE_DAYS)
}

#[derive(Clone, Copy)]
pub struct AsOf<'a> {
    cache: &'a Cache,
    date: NaiveDate,
}

impl<'a> AsOf<'a> {
    pub fn new(cache: &'a Cache, date: NaiveDate) -> Self {
        Self { cache, date }
    }

    pub fn date(&self) -> NaiveDate {
        self.date
    }

    /// Whether this view describes the past (see [`is_historical`]).
    pub fn is_historical(&self) -> bool {
        is_historical(self.date)
    }

    /// Daily bars from `lookback_days` before the view date through the view date.
    pub fn price_bars(&self, ticker: &str, lookback_days: i64) -> Result<Vec<PriceBar>> {
        let mut bars = self
            .cache
            .get_price_bars(ticker, self.date - Duration::days(lookback_days), self.date)?;
        bars.retain(|b| b.date <= self.date);
        Ok(bars)
    }

    /// The last bar we could have seen.
    pub fn last_bar(&self, ticker: &str) -> Option<PriceBar> {
        self.price_bars(ticker, 10).ok()?.into_iter().last()
    }

    /// Fundamentals as of the view date. For a historical view only rows
    /// built from dated SEC filings are eligible.
    pub fn fundamentals(&self, ticker: &str) -> Result<Option<FundamentalSnapshot>> {
        self.cache.get_fundamentals(ticker, self.date, self.is_historical())
    }

    /// Insider trades in the window whose filing was public by the view date.
    pub fn insider_trades(&self, ticker: &str, days: i64) -> Result<Vec<InsiderTrade>> {
        let mut v = self
            .cache
            .get_insider_trades(ticker, self.date - Duration::days(days), self.date)?;
        v.retain(|t| t.filing_date <= self.date && t.trade_date <= self.date);
        Ok(v)
    }

    pub fn news_items(&self, ticker: &str, days: i64) -> Result<Vec<NewsItem>> {
        let mut v = self
            .cache
            .get_news_items(ticker, self.date - Duration::days(days), self.date)?;
        v.retain(|n| n.article_date <= self.date);
        Ok(v)
    }

    pub fn reddit_snapshots(&self, ticker: &str, days: i64) -> Result<Vec<RedditSnapshot>> {
        let mut v = self
            .cache
            .get_reddit_snapshots(ticker, self.date - Duration::days(days), self.date)?;
        v.retain(|r| r.fetch_date <= self.date);
        Ok(v)
    }

    pub fn macro_points(&self, series_id: &str, lookback_days: i64) -> Result<Vec<MacroDataPoint>> {
        let mut v = self
            .cache
            .get_macro_data(series_id, self.date - Duration::days(lookback_days), self.date)?;
        v.retain(|p| p.date <= self.date);
        Ok(v)
    }

    /// SEC facts filed on or before the view date.
    pub fn pit_facts(&self, ticker: &str) -> Result<Vec<PitFact>> {
        let mut v = self.cache.get_pit_facts(ticker, self.date)?;
        v.retain(|f| f.filed <= self.date);
        Ok(v)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::source::FundamentalSnapshot;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn bar(date: &str, close: f64) -> PriceBar {
        PriceBar { date: d(date), open: close, high: close, low: close, close, adj_close: close, volume: 1 }
    }

    fn fundamentals(ticker: &str, date: &str) -> FundamentalSnapshot {
        FundamentalSnapshot {
            ticker: ticker.into(),
            date: d(date),
            revenue_ttm: Some(1.0),
            revenue_cagr_3yr: None,
            net_margin_pct: None,
            debt_to_equity: None,
            price_to_book: None,
            price_return_12m_1m: None,
            market_share_proxy: None,
            operating_cashflow: None,
            return_on_assets: None,
            gross_profit_margin: None,
        }
    }

    /// Cache with data on BOTH sides of 2023-06-30.
    fn seeded() -> Cache {
        let c = Cache::open(":memory:").unwrap();
        c.insert_price_bars(
            "X",
            &[bar("2023-06-28", 10.0), bar("2023-06-30", 11.0), bar("2023-07-03", 12.0), bar("2023-08-01", 99.0)],
        )
        .unwrap();
        c.insert_insider_trades(&[
            InsiderTrade { ticker: "X".into(), filing_date: d("2023-06-20"), trade_date: d("2023-06-18"), insider_name: "a".into(), insider_role: "CEO".into(), shares: 1.0, transaction_type: "A".into() },
            // Traded before the date but FILED after: not public yet.
            InsiderTrade { ticker: "X".into(), filing_date: d("2023-07-05"), trade_date: d("2023-06-29"), insider_name: "b".into(), insider_role: "CFO".into(), shares: 1.0, transaction_type: "A".into() },
            InsiderTrade { ticker: "X".into(), filing_date: d("2023-07-10"), trade_date: d("2023-07-08"), insider_name: "c".into(), insider_role: "CFO".into(), shares: 1.0, transaction_type: "A".into() },
        ])
        .unwrap();
        c.insert_news_items(&[
            NewsItem { ticker: "X".into(), article_date: d("2023-06-25"), tone: 1.0, headline: "past".into(), source: "s".into() },
            NewsItem { ticker: "X".into(), article_date: d("2023-07-02"), tone: 1.0, headline: "future".into(), source: "s".into() },
        ])
        .unwrap();
        // Collected on the day they are labelled with (valid), on both sides of the date.
        for (date, sub) in [("2023-06-29", "wsb"), ("2023-07-04", "stocks")] {
            let snap = RedditSnapshot { ticker: "X".into(), fetch_date: d(date), subreddit: sub.into(), mention_count: 5, avg_upvote_ratio: 0.8, total_comments: 1 };
            c.insert_reddit_snapshot_at(&snap, Some(&format!("{date} 09:00:00"))).unwrap();
        }
        c.insert_macro_data(&[
            MacroDataPoint { series_id: "VIXCLS".into(), date: d("2023-06-29"), value: 14.0 },
            MacroDataPoint { series_id: "VIXCLS".into(), date: d("2023-07-05"), value: 40.0 },
        ])
        .unwrap();
        c.insert_pit_facts(
            "X",
            &[
                PitFact { concept: "assets".into(), start: None, end: d("2023-03-31"), value: 1.0, filed: d("2023-05-01"), form: "10-Q".into() },
                PitFact { concept: "assets".into(), start: None, end: d("2023-06-30"), value: 2.0, filed: d("2023-08-01"), form: "10-Q".into() },
            ],
        )
        .unwrap();
        c
    }

    #[test]
    fn nothing_after_the_as_of_date_is_ever_returned() {
        let c = seeded();
        let v = AsOf::new(&c, d("2023-06-30"));

        assert!(v.price_bars("X", 365).unwrap().iter().all(|b| b.date <= d("2023-06-30")));
        assert_eq!(v.price_bars("X", 365).unwrap().len(), 2);
        assert_eq!(v.last_bar("X").unwrap().close, 11.0);

        let insiders = v.insider_trades("X", 365).unwrap();
        assert_eq!(insiders.len(), 1, "filed-after and future trades must be hidden");
        assert_eq!(insiders[0].insider_name, "a");

        let news = v.news_items("X", 365).unwrap();
        assert_eq!(news.len(), 1);
        assert_eq!(news[0].headline, "past");

        let reddit = v.reddit_snapshots("X", 365).unwrap();
        assert_eq!(reddit.len(), 1);
        assert!(reddit.iter().all(|r| r.fetch_date <= d("2023-06-30")));

        let vix = v.macro_points("VIXCLS", 365).unwrap();
        assert_eq!(vix.len(), 1);
        assert_eq!(vix[0].value, 14.0);

        let facts = v.pit_facts("X").unwrap();
        assert_eq!(facts.len(), 1, "the 2023-06-30 balance sheet was filed in August");
        assert_eq!(facts[0].value, 1.0);
    }

    #[test]
    fn moving_the_view_forward_reveals_more_but_only_what_had_happened() {
        let c = seeded();
        let later = AsOf::new(&c, d("2023-07-06"));
        assert_eq!(later.insider_trades("X", 365).unwrap().len(), 2);
        assert_eq!(later.price_bars("X", 365).unwrap().len(), 3);
        assert_eq!(later.pit_facts("X").unwrap().len(), 1);
        let latest = AsOf::new(&c, d("2023-09-01"));
        assert_eq!(latest.pit_facts("X").unwrap().len(), 2);
    }

    #[test]
    fn a_view_of_the_past_cannot_see_a_current_snapshot_of_fundamentals() {
        let c = Cache::open(":memory:").unwrap();
        // The old pipeline stored today's Yahoo numbers under a historical date.
        c.insert_fundamentals(&fundamentals("X", "2020-01-31"), "yahoo_current").unwrap();
        // ...and older rows have no provenance at all.
        c.insert_fundamentals(&fundamentals("Y", "2020-01-31"), "legacy").unwrap();
        assert!(AsOf::new(&c, d("2020-01-31")).fundamentals("X").unwrap().is_none());
        assert!(AsOf::new(&c, d("2020-01-31")).fundamentals("Y").unwrap().is_none());

        // A row built from dated filings IS allowed.
        c.insert_fundamentals(&fundamentals("Z", "2020-01-31"), "sec_pit").unwrap();
        assert!(AsOf::new(&c, d("2020-01-31")).fundamentals("Z").unwrap().is_some());
    }

    #[test]
    fn reddit_rows_collected_long_after_their_label_are_hidden() {
        // A historical run stamped TODAY's Reddit search results with an old
        // date. Such a row is labelled 2023-06-29 but was really fetched now.
        let c = Cache::open(":memory:").unwrap();
        let fake = RedditSnapshot { ticker: "X".into(), fetch_date: d("2023-06-29"), subreddit: "wsb".into(), mention_count: 99, avg_upvote_ratio: 0.9, total_comments: 1 };
        c.insert_reddit_snapshot(&fake).unwrap(); // fetched_at = now
        assert!(AsOf::new(&c, d("2023-06-30")).reddit_snapshots("X", 30).unwrap().is_empty());

        // The same row collected on its own date is genuine.
        c.insert_reddit_snapshot_at(&fake, Some("2023-06-29 08:00:00")).unwrap();
        assert_eq!(AsOf::new(&c, d("2023-06-30")).reddit_snapshots("X", 30).unwrap().len(), 1);
    }

    #[test]
    fn a_live_view_accepts_a_current_snapshot() {
        let c = Cache::open(":memory:").unwrap();
        let today = Local::now().date_naive();
        c.insert_fundamentals(&fundamentals("X", &today.to_string()), "yahoo_current").unwrap();
        let v = AsOf::new(&c, today);
        assert!(!v.is_historical());
        assert!(v.fundamentals("X").unwrap().is_some());
    }

    #[test]
    fn historical_boundary() {
        let today = Local::now().date_naive();
        assert!(is_historical(today - Duration::days(LIVE_GRACE_DAYS + 1)));
        assert!(!is_historical(today - Duration::days(LIVE_GRACE_DAYS)));
        assert!(!is_historical(today));
    }

    #[test]
    fn correlation_matrix_lookup_is_bound_to_the_as_of_date() {
        use crate::data::types::IndustryCorrelation;
        let c = Cache::open(":memory:").unwrap();
        let mk = |date: &str, r: f64| IndustryCorrelation { industry_a: "A".into(), industry_b: "B".into(), correlation: r, date: d(date), window_days: 60 };
        c.insert_industry_correlations(&[mk("2023-06-01", 0.5)]).unwrap();
        c.insert_industry_correlations(&[mk("2024-06-01", 0.9)]).unwrap(); // the "future"

        let got = c.get_industry_correlations_asof(60, d("2023-06-20"), 30).unwrap();
        assert_eq!(got.len(), 1);
        assert!((got[0].correlation - 0.5).abs() < 1e-12, "must not load the 2024 matrix");
        // Too old to use at that date.
        assert!(c.get_industry_correlations_asof(60, d("2023-09-01"), 30).unwrap().is_empty());
        // Nothing existed yet.
        assert!(c.get_industry_correlations_asof(60, d("2023-01-01"), 30).unwrap().is_empty());
    }
}
