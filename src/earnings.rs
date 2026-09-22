//! Earnings-date awareness: don't rebalance a name right before it reports.
//!
//! LIVE-only by construction. Yahoo's `calendarEvents` module reports what it
//! currently believes is the *next* earnings date for a ticker — there is no
//! point-in-time history of past earnings-date announcements available here,
//! so this cannot be used inside a backtest without either looking ahead or
//! simply being wrong about what was knowable on a historical date. It is
//! wired into the live `--morning` job only, the same way the macro regime
//! snapshot is live-only.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EarningsFlag {
    pub ticker: String,
    pub nearest_date: NaiveDate,
    /// Negative: earnings already happened this many days ago. Positive:
    /// earnings is this many days away.
    pub days_away: i64,
}

/// True if any known earnings date falls within `window_days` of `as_of`,
/// in either direction (a surprise a few days ago is still a reason for
/// caution, not just an upcoming one).
pub fn is_near_earnings(as_of: NaiveDate, earnings_dates: &[NaiveDate], window_days: i64) -> bool {
    earnings_dates.iter().any(|d| (*d - as_of).num_days().abs() <= window_days)
}

/// The nearest earnings date to `as_of` (by absolute distance), if any.
pub fn nearest_earnings(as_of: NaiveDate, earnings_dates: &[NaiveDate]) -> Option<NaiveDate> {
    earnings_dates.iter().copied().min_by_key(|d| (*d - as_of).num_days().abs())
}

/// Flag every (ticker, earnings_dates) pair whose nearest date falls within
/// `window_days` of `as_of`, sorted by proximity (soonest/most-recent first).
pub fn flag_near_earnings(
    as_of: NaiveDate,
    per_ticker: &[(String, Vec<NaiveDate>)],
    window_days: i64,
) -> Vec<EarningsFlag> {
    let mut out: Vec<EarningsFlag> = per_ticker
        .iter()
        .filter_map(|(ticker, dates)| {
            let nearest = nearest_earnings(as_of, dates)?;
            is_near_earnings(as_of, dates, window_days).then(|| EarningsFlag {
                ticker: ticker.clone(),
                nearest_date: nearest,
                days_away: (nearest - as_of).num_days(),
            })
        })
        .collect();
    out.sort_by_key(|f| f.days_away.abs());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn flags_dates_within_the_window_on_either_side() {
        let dates = [d("2024-01-20")];
        assert!(is_near_earnings(d("2024-01-15"), &dates, 5), "5 days before");
        assert!(is_near_earnings(d("2024-01-25"), &dates, 5), "5 days after");
        assert!(!is_near_earnings(d("2024-01-13"), &dates, 5), "6 days before: out of window");
        assert!(!is_near_earnings(d("2024-01-27"), &dates, 5), "7 days after: out of window");
    }

    #[test]
    fn exactly_on_the_earnings_date_counts() {
        let dates = [d("2024-01-20")];
        assert!(is_near_earnings(d("2024-01-20"), &dates, 3));
    }

    #[test]
    fn no_dates_is_never_flagged() {
        assert!(!is_near_earnings(d("2024-01-20"), &[], 30));
        assert!(nearest_earnings(d("2024-01-20"), &[]).is_none());
    }

    #[test]
    fn nearest_picks_the_closest_of_several_candidates() {
        let dates = [d("2024-01-01"), d("2024-06-01"), d("2024-01-25")];
        let n = nearest_earnings(d("2024-01-20"), &dates).unwrap();
        assert_eq!(n, d("2024-01-25"));
    }

    #[test]
    fn flag_near_earnings_sorts_by_proximity_and_skips_tickers_with_no_dates() {
        let per_ticker = vec![
            ("FAR".to_string(), vec![d("2024-03-01")]),
            ("SOON".to_string(), vec![d("2024-01-21")]),
            ("NONE".to_string(), vec![]),
            ("TODAY".to_string(), vec![d("2024-01-20")]),
        ];
        let flags = flag_near_earnings(d("2024-01-20"), &per_ticker, 5);
        let tickers: Vec<&str> = flags.iter().map(|f| f.ticker.as_str()).collect();
        assert_eq!(tickers, vec!["TODAY", "SOON"]);
        assert_eq!(flags[0].days_away, 0);
        assert_eq!(flags[1].days_away, 1);
    }
}
