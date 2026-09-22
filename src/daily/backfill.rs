//! Replay the daily job over past dates, point-in-time.
//!
//! This is a BACKTEST, not forward evidence: every pick is computed from data
//! available on its own date (the `AsOf` layer), so it is free of look-ahead
//! from our data, but the model, universe and parameters were all chosen with
//! today's knowledge (and the universe is today's S&P 500: survivorship).
//! Results go to a separate directory so they can never be mistaken for, or
//! mixed into, the real forward log.
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
use std::path::Path;
use std::time::Duration as StdDuration;
use tracing::{info, warn};

use crate::data::asof::LIVE_GRACE_DAYS;
use crate::data::cache::Cache;
use crate::forward_test::{self, LogInput};
use crate::gics::GicsTaxonomy;
use crate::signals::PickingEngine;

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

    let universe = build_auto_universe(&cache, taxonomy, us_only).await?;
    let engine = PickingEngine::new(cache.clone());
    let threshold = min_score_threshold();

    let (mut written, mut skipped_closed, mut failed, mut timed_out) = (0usize, 0usize, 0usize, 0usize);
    let first_day = last_day - Duration::days(days);
    let mut date = first_day;
    while date <= last_day {
        if matches!(date.weekday(), Weekday::Sat | Weekday::Sun) {
            date += Duration::days(1);
            continue;
        }

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
        "Backfill {first_day} -> {last_day} (clamped to stay {} day(s) before today): \
         {written} day(s) recorded in {}, {skipped_closed} closed-market day(s) skipped, \
         {failed} failed, {timed_out} exceeded the per-day budget",
        LIVE_GRACE_DAYS + 1,
        dir.display(),
    ))
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
