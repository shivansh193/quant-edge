//! Indexed, date-sorted price series.
//!
//! Lookups are `O(log n)` binary searches instead of scanning a hash map, and
//! every accessor is explicit about which side of a date it may return:
//!
//!   * `on_or_before` — the last bar we could have *known* at `date`.
//!   * `strictly_after` — the next tradable bar (used for next-bar fills).
//!
//! There is deliberately no accessor that silently mixes the two.

use chrono::NaiveDate;
use std::collections::HashMap;

use super::source::PriceBar;

#[derive(Debug, Clone, Default)]
pub struct PriceSeries {
    bars: Vec<PriceBar>,
}

impl PriceSeries {
    /// Sorts by date and keeps the last bar for any duplicated date.
    /// Bars with a non-positive or non-finite adjusted close are dropped.
    pub fn new(mut bars: Vec<PriceBar>) -> Self {
        bars.retain(|b| b.adj_close.is_finite() && b.adj_close > 0.0);
        bars.sort_by_key(|b| b.date);
        let mut deduped: Vec<PriceBar> = Vec::with_capacity(bars.len());
        for b in bars {
            match deduped.last_mut() {
                Some(last) if last.date == b.date => *last = b,
                _ => deduped.push(b),
            }
        }
        Self { bars: deduped }
    }

    pub fn len(&self) -> usize {
        self.bars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bars.is_empty()
    }

    pub fn bars(&self) -> &[PriceBar] {
        &self.bars
    }

    pub fn first_date(&self) -> Option<NaiveDate> {
        self.bars.first().map(|b| b.date)
    }

    pub fn last_date(&self) -> Option<NaiveDate> {
        self.bars.last().map(|b| b.date)
    }

    /// Bar dated exactly `date`, if the market traded that day.
    pub fn on(&self, date: NaiveDate) -> Option<&PriceBar> {
        let i = self.bars.partition_point(|b| b.date < date);
        self.bars.get(i).filter(|b| b.date == date)
    }

    /// Latest bar dated `<= date` (forward-fill semantics).
    pub fn on_or_before(&self, date: NaiveDate) -> Option<&PriceBar> {
        let i = self.bars.partition_point(|b| b.date <= date);
        i.checked_sub(1).and_then(|k| self.bars.get(k))
    }

    /// First bar dated `> date` — the next bar we could actually trade on.
    pub fn strictly_after(&self, date: NaiveDate) -> Option<&PriceBar> {
        let i = self.bars.partition_point(|b| b.date <= date);
        self.bars.get(i)
    }

    /// All bars with `from <= date <= to`.
    pub fn window(&self, from: NaiveDate, to: NaiveDate) -> &[PriceBar] {
        let lo = self.bars.partition_point(|b| b.date < from);
        let hi = self.bars.partition_point(|b| b.date <= to);
        if lo >= hi {
            &[]
        } else {
            &self.bars[lo..hi]
        }
    }

    /// The last `n` bars dated `<= date`.
    pub fn last_n_through(&self, date: NaiveDate, n: usize) -> &[PriceBar] {
        let hi = self.bars.partition_point(|b| b.date <= date);
        let lo = hi.saturating_sub(n);
        &self.bars[lo..hi]
    }

    /// Average daily dollar volume over the last `n` bars through `date`.
    pub fn avg_dollar_volume(&self, date: NaiveDate, n: usize) -> Option<f64> {
        let w = self.last_n_through(date, n);
        if w.is_empty() {
            return None;
        }
        Some(w.iter().map(|b| b.close * b.volume as f64).sum::<f64>() / w.len() as f64)
    }

    /// Standard deviation of daily log returns over the last `n` bars through
    /// `date` (needs at least 5 returns).
    pub fn daily_volatility(&self, date: NaiveDate, n: usize) -> Option<f64> {
        let w = self.last_n_through(date, n + 1);
        if w.len() < 6 {
            return None;
        }
        let rets: Vec<f64> = w
            .windows(2)
            .map(|p| (p[1].adj_close / p[0].adj_close).ln())
            .collect();
        let m = rets.iter().sum::<f64>() / rets.len() as f64;
        let var = rets.iter().map(|r| (r - m).powi(2)).sum::<f64>() / (rets.len() - 1) as f64;
        Some(var.sqrt())
    }
}

/// 12-1 month price return from ~13 months of bars: the return from the first
/// bar to the bar one month (about 21 trading days) before the last, skipping
/// the most recent month to avoid short-term reversal.
pub fn momentum_12m1m(bars: &[PriceBar]) -> Option<f64> {
    if bars.len() < 50 {
        return None;
    }
    let skip = 21.min(bars.len() / 10);
    let end_idx = bars.len().saturating_sub(skip + 1);
    let start = bars[0].adj_close;
    if start <= 0.0 {
        return None;
    }
    Some((bars[end_idx].adj_close - start) / start)
}

/// Price series for many tickers.
#[derive(Debug, Clone, Default)]
pub struct PriceStore {
    series: HashMap<String, PriceSeries>,
}

impl PriceStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, ticker: impl Into<String>, series: PriceSeries) {
        if !series.is_empty() {
            self.series.insert(ticker.into(), series);
        }
    }

    pub fn from_bars(map: HashMap<String, Vec<PriceBar>>) -> Self {
        let mut s = Self::new();
        for (t, bars) in map {
            s.insert(t, PriceSeries::new(bars));
        }
        s
    }

    pub fn get(&self, ticker: &str) -> Option<&PriceSeries> {
        self.series.get(ticker)
    }

    pub fn tickers(&self) -> impl Iterator<Item = &String> {
        self.series.keys()
    }

    pub fn len(&self) -> usize {
        self.series.len()
    }

    pub fn is_empty(&self) -> bool {
        self.series.is_empty()
    }

    /// Sorted union of every trading date in `[from, to]` across all tickers.
    pub fn calendar(&self, from: NaiveDate, to: NaiveDate) -> Vec<NaiveDate> {
        let mut set = std::collections::BTreeSet::new();
        for s in self.series.values() {
            for b in s.window(from, to) {
                set.insert(b.date);
            }
        }
        set.into_iter().collect()
    }
}

#[cfg(test)]
pub(crate) fn test_bar(date: &str, close: f64) -> PriceBar {
    PriceBar {
        date: date.parse().unwrap(),
        open: close,
        high: close,
        low: close,
        close,
        adj_close: close,
        volume: 1_000_000,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn series() -> PriceSeries {
        // Fri, Mon, Tue — the weekend has no bars.
        PriceSeries::new(vec![
            test_bar("2024-01-05", 10.0),
            test_bar("2024-01-08", 11.0),
            test_bar("2024-01-09", 12.0),
        ])
    }

    #[test]
    fn on_or_before_forward_fills_over_a_weekend() {
        let s = series();
        assert_eq!(s.on_or_before(d("2024-01-07")).unwrap().close, 10.0);
        assert_eq!(s.on_or_before(d("2024-01-08")).unwrap().close, 11.0);
        assert!(s.on_or_before(d("2024-01-04")).is_none());
    }

    #[test]
    fn strictly_after_never_returns_the_same_day() {
        let s = series();
        assert_eq!(s.strictly_after(d("2024-01-05")).unwrap().date, d("2024-01-08"));
        assert_eq!(s.strictly_after(d("2024-01-07")).unwrap().date, d("2024-01-08"));
        assert!(s.strictly_after(d("2024-01-09")).is_none());
    }

    #[test]
    fn on_requires_an_exact_trading_day() {
        let s = series();
        assert!(s.on(d("2024-01-06")).is_none());
        assert_eq!(s.on(d("2024-01-09")).unwrap().close, 12.0);
    }

    #[test]
    fn unsorted_duplicate_and_bad_bars_are_cleaned() {
        let s = PriceSeries::new(vec![
            test_bar("2024-01-09", 12.0),
            test_bar("2024-01-05", 10.0),
            test_bar("2024-01-05", 10.5), // duplicate date: last one wins
            test_bar("2024-01-08", 0.0),  // invalid
            test_bar("2024-01-08", f64::NAN),
        ]);
        assert_eq!(s.len(), 2);
        assert_eq!(s.on(d("2024-01-05")).unwrap().close, 10.5);
    }

    #[test]
    fn window_is_inclusive_on_both_ends() {
        let s = series();
        assert_eq!(s.window(d("2024-01-05"), d("2024-01-08")).len(), 2);
        assert!(s.window(d("2024-01-10"), d("2024-01-12")).is_empty());
        assert!(s.window(d("2024-01-09"), d("2024-01-05")).is_empty());
    }

    #[test]
    fn calendar_is_the_sorted_union() {
        let mut st = PriceStore::new();
        st.insert("A", PriceSeries::new(vec![test_bar("2024-01-05", 1.0), test_bar("2024-01-09", 1.0)]));
        st.insert("B", PriceSeries::new(vec![test_bar("2024-01-08", 1.0)]));
        assert_eq!(
            st.calendar(d("2024-01-01"), d("2024-12-31")),
            vec![d("2024-01-05"), d("2024-01-08"), d("2024-01-09")]
        );
    }

    #[test]
    fn dollar_volume_and_vol() {
        let bars: Vec<PriceBar> = (0..30)
            .map(|i| {
                let date = d("2024-01-01") + chrono::Duration::days(i);
                let mut b = test_bar(&date.to_string(), 100.0 + (i % 3) as f64);
                b.volume = 10;
                b
            })
            .collect();
        let s = PriceSeries::new(bars);
        let adv = s.avg_dollar_volume(d("2024-02-15"), 20).unwrap();
        assert!(adv > 900.0 && adv < 1100.0);
        assert!(s.daily_volatility(d("2024-02-15"), 20).unwrap() > 0.0);
        assert!(s.daily_volatility(d("2024-01-03"), 20).is_none());
    }
}
