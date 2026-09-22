//! Replay the daily job over past dates, point-in-time.
//!
//! This is a BACKTEST, not forward evidence: every pick is computed from data
//! available on its own date (the `AsOf` layer), so it is free of look-ahead
//! from our data, and (for `--us`) the universe on each day is that day's
//! *actual* S&P 500 membership, not today's — see
//! `universe::historical_membership`. What's still chosen with today's
//! knowledge is the model and its parameters. Results go to a separate
//! directory so they can never be mistaken for, or mixed into, the real
//! forward log.
//!
//! A backfill date is always in the past by construction, so it must never
//! trigger a *live* fetch (fresh EDGAR/Reddit/etc. calls describing "now"):
//! that would be pointless (the data wouldn't describe the backfilled date
//! anyway) and, if a live source is slow or blocking, can make a single day
//! take hours instead of seconds. `AsOf::is_historical` normally leaves a
//! `LIVE_GRACE_DAYS` window open for the real daily job; a backfill run
//! clamps its range to stay outside that window entirely, and each day gets a
//! hard wall-clock budget so a stuck network call can never stall the run.

use anyhow::Result;
use chrono::{Datelike, Duration, Local, NaiveDate, Weekday};
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration as StdDuration;
use tracing::{info, warn};

use crate::data::asof::LIVE_GRACE_DAYS;
use crate::data::cache::Cache;
use crate::data::yahoo::YahooFinance;
use crate::forward_test::{self, LogInput};
use crate::gics::GicsTaxonomy;
use crate::signals::PickingEngine;
use crate::universe::builder::{CapFilter, Market, Universe, UniverseBuilder, UniverseConfig};
use crate::universe::HistoricalMembership;

use super::{build_auto_universe, min_score_threshold, score_based_picks};

pub const BACKFILL_DIR: &str = "backfill_log";
pub const BACKFILL_LABEL: &str = "BACKFILL: point-in-time replay (a backtest, NOT forward evidence)";
/// Give up on a single day rather than let one bad network call stall the run.
const PER_DAY_BUDGET: StdDuration = StdDuration::from_secs(180);

/// Score every trading day in `(last_day - days) ..= last_day` and record each
/// into `dir`. Weekends, market holidays (no bar dated that day), and any date
/// too recent to be reliably historical are skipped. `last_day` is clamped so
/// the run never enters the live-fetch window regardless of how it's called.
pub async fn run_backfill(
    cache: Cache,
    taxonomy: &GicsTaxonomy,
    last_day: NaiveDate,
    days: i64,
    us_only: bool,
    dir: &Path,
) -> Result<String> {
    let latest_safe = Local::now().date_naive() - Duration::days(LIVE_GRACE_DAYS + 1);
    let last_day = last_day.min(latest_safe);
    let first_day = last_day - Duration::days(days);

    // Survivorship bias: today's constituent list is wrong for a past date
    // (every name removed since is invisible). For a US-only backfill, use
    // each day's ACTUAL point-in-time S&P 500 membership instead. Resolve one
    // superset universe covering every ticker that was ever a member across
    // the window (cheap for a short window: the index rarely changes month
    // to month), then filter it per day in memory - no extra network calls.
    let (superset, membership_by_day, survivorship_corrected): (Universe, Vec<(NaiveDate, HashSet<String>)>, bool) = if us_only {
        match build_point_in_time_superset(&cache, taxonomy, first_day, last_day).await {
            Ok((u, by_day)) => (u, by_day, true),
            Err(e) => {
                warn!("point-in-time S&P 500 membership unavailable, falling back to today's list: {e:#}");
                let u = build_auto_universe(&cache, taxonomy, us_only).await?;
                let all: HashSet<String> = u.tickers().into_iter().collect();
                (u, vec![(first_day, all)], false)
            }
        }
    } else {
        let u = build_auto_universe(&cache, taxonomy, us_only).await?;
        let all: HashSet<String> = u.tickers().into_iter().collect();
        (u, vec![(first_day, all)], false)
    };

    let engine = PickingEngine::new(cache.clone());
    let threshold = min_score_threshold();

    let (mut written, mut skipped_closed, mut failed, mut timed_out) = (0usize, 0usize, 0usize, 0usize);
    let mut date = first_day;
    while date <= last_day {
        if matches!(date.weekday(), Weekday::Sat | Weekday::Sun) {
            date += Duration::days(1);
            continue;
        }

        let members = membership_by_day
            .iter()
            .filter(|(d, _)| *d <= date)
            .max_by_key(|(d, _)| *d)
            .map(|(_, set)| set.clone())
            .unwrap_or_default();
        let universe = superset.filtered(&members);

        match tokio::time::timeout(PER_DAY_BUDGET, engine.rank_universe(&universe, date)).await {
            Ok(Ok(scores)) if !scores.is_empty() => {
                let through = forward_test::data_through(&cache, &scores, date);
                if through != date {
                    // No bar dated `date`: the market was closed.
                    skipped_closed += 1;
                } else {
                    let picks = score_based_picks(&scores, threshold);
                    let input = LogInput {
                        as_of: date,
                        data_through: through,
                        strategy: BACKFILL_LABEL,
                        scores: &scores,
                        picks: &picks,
                        vix: None,
                    };
                    match forward_test::record(dir, &input) {
                        Ok(_) => {
                            written += 1;
                            info!(date = %date, picks = picks.len(), scored = scores.len(), "backfilled");
                        }
                        Err(e) => {
                            failed += 1;
                            warn!(date = %date, "not recorded: {e:#}");
                        }
                    }
                }
            }
            Ok(Ok(_)) => failed += 1,
            Ok(Err(e)) => {
                failed += 1;
                warn!(date = %date, "ranking failed: {e:#}");
            }
            Err(_) => {
                timed_out += 1;
                warn!(date = %date, "exceeded the {:?} per-day budget - skipped, not stalled", PER_DAY_BUDGET);
            }
        }
        date += Duration::days(1);
    }

    Ok(format!(
        "Backfill {first_day} -> {last_day} (clamped to stay {} day(s) before today, \
         universe: {}): {written} day(s) recorded in {}, {skipped_closed} closed-market day(s) \
         skipped, {failed} failed, {timed_out} exceeded the per-day budget",
        LIVE_GRACE_DAYS + 1,
        if survivorship_corrected { "point-in-time S&P 500 membership" } else { "today's list (not survivorship-corrected)" },
        dir.display(),
    ))
}

/// Resolve one superset `Universe` covering every ticker that was an S&P 500
/// member at any point in `[from, to]`, plus the membership timeline needed
/// to filter it down to each individual day.
async fn build_point_in_time_superset(
    cache: &Cache,
    taxonomy: &GicsTaxonomy,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<(Universe, Vec<(NaiveDate, HashSet<String>)>)> {
    let hist = HistoricalMembership::new(cache.clone());
    let union = hist.union_over(from, to).await?;
    anyhow::ensure!(!union.is_empty(), "empty point-in-time membership union");

    // Per-day lookups need each change-event's effective set, not just the
    // union - fetch each day the underlying dataset actually changes within
    // the window (cheap: a month has at most a handful of such events).
    let mut by_day: Vec<(NaiveDate, HashSet<String>)> = Vec::new();
    let start_members = hist.members_as_of(from).await?;
    by_day.push((from, start_members.into_iter().collect()));
    let mut d = from;
    while d <= to {
        let members = hist.members_as_of(d).await?;
        if by_day.last().map_or(true, |(_, prev)| prev != &members.iter().cloned().collect::<HashSet<_>>()) {
            by_day.push((d, members.into_iter().collect()));
        }
        d += Duration::days(1);
    }

    let tickers: Vec<String> = union.into_iter().collect();
    let source = YahooFinance::new(cache.clone());
    let ub = UniverseBuilder::new(&source, taxonomy);
    let config = UniverseConfig {
        market: Market::NYSE,
        cap_filter: CapFilter::Mixed,
        n_industries: 100,
        exclude_industry_codes: vec![],
    };
    let mut universe = ub.build_from_tickers(tickers, config).await?;
    ub.enrich_gics(&mut universe).await?;
    universe.trim_to_n_industries(100, &[]);

    Ok((universe, by_day))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_day_is_clamped_outside_the_live_window() {
        let today = Local::now().date_naive();
        let requested = today; // "run through today" - would have been live
        let clamped = requested.min(today - Duration::days(LIVE_GRACE_DAYS + 1));
        assert!(clamped <= today - Duration::days(LIVE_GRACE_DAYS + 1));
        assert!(clamped < requested);
    }
}
