//! Decision journal: your own discretionary calls, logged with a thesis
//! *before* the outcome is known, closed and scored later.
//!
//! This is the one piece of the tool that measures *you*, not the model. The
//! forward-test log (`forward_test`) already records what the model would
//! have done every day; this records what you actually chose to do and why,
//! so the two can be compared. Discipline matters more than mechanism here —
//! nothing computes a thesis for you, and nothing stops you from logging a
//! bad one. The value is in the habit of writing the reasoning down before
//! you know if it was right.

use anyhow::{Context, Result};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::data::cache::Cache;

#[derive(Debug, Clone)]
pub struct NewDecision {
    pub ticker: String,
    pub entry_date: NaiveDate,
    pub thesis: String,
    pub expected_holding_days: u32,
    pub entry_price: f64,
    /// The model's composite score for this ticker around the entry date, if
    /// one was found in the forward/backfill logs — lets you see, later,
    /// whether your call agreed or disagreed with the model.
    pub model_composite_at_entry: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub id: i64,
    pub ticker: String,
    pub entry_date: NaiveDate,
    pub thesis: String,
    pub expected_holding_days: u32,
    pub entry_price: f64,
    pub model_composite_at_entry: Option<f64>,
    pub status: String, // "open" | "closed"
    pub exit_date: Option<NaiveDate>,
    pub exit_price: Option<f64>,
    pub outcome_notes: Option<String>,
}

impl JournalEntry {
    pub fn is_open(&self) -> bool {
        self.status == "open"
    }

    /// Calendar days since entry, as of `today`. Negative is meaningless and
    /// clamped to 0 (a future-dated entry shouldn't report as "overdue").
    pub fn days_held(&self, today: NaiveDate) -> i64 {
        (today - self.entry_date).num_days().max(0)
    }

    /// True once `expected_holding_days` has elapsed and the position is
    /// still open — a candidate for `--journal-score` to prompt closing.
    pub fn is_due(&self, today: NaiveDate) -> bool {
        self.is_open() && self.days_held(today) >= self.expected_holding_days as i64
    }

    /// Realised return once closed. `None` while still open.
    pub fn return_pct(&self) -> Option<f64> {
        let exit = self.exit_price?;
        (self.entry_price > 0.0).then(|| (exit / self.entry_price - 1.0) * 100.0)
    }

    /// Whether your call and the model's composite pointed the same way at
    /// entry (model >= 60 = the model would also have leaned long). `None`
    /// if no model score was found for that ticker/date.
    pub fn agreed_with_model(&self) -> Option<bool> {
        self.model_composite_at_entry.map(|c| c >= 60.0)
    }
}

/// Summary across a set of closed entries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JournalSummary {
    pub n_closed: usize,
    pub mean_return_pct: f64,
    pub hit_rate_pct: f64, // share with positive return
    pub n_agreed_with_model: usize,
    pub n_disagreed_with_model: usize,
    pub mean_return_when_agreed_pct: f64,
    pub mean_return_when_disagreed_pct: f64,
}

pub fn summarize(entries: &[JournalEntry]) -> JournalSummary {
    let closed: Vec<&JournalEntry> = entries.iter().filter(|e| !e.is_open()).collect();
    if closed.is_empty() {
        return JournalSummary::default();
    }
    let returns: Vec<f64> = closed.iter().filter_map(|e| e.return_pct()).collect();
    let mean = |v: &[f64]| if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 };

    let agreed: Vec<f64> = closed.iter().filter(|e| e.agreed_with_model() == Some(true)).filter_map(|e| e.return_pct()).collect();
    let disagreed: Vec<f64> = closed.iter().filter(|e| e.agreed_with_model() == Some(false)).filter_map(|e| e.return_pct()).collect();

    JournalSummary {
        n_closed: closed.len(),
        mean_return_pct: mean(&returns),
        hit_rate_pct: if returns.is_empty() { 0.0 } else {
            returns.iter().filter(|r| **r > 0.0).count() as f64 / returns.len() as f64 * 100.0
        },
        n_agreed_with_model: agreed.len(),
        n_disagreed_with_model: disagreed.len(),
        mean_return_when_agreed_pct: mean(&agreed),
        mean_return_when_disagreed_pct: mean(&disagreed),
    }
}

/// Look up the model's composite score for `ticker` from whichever
/// forward-test log entry is closest to (and not after) `entry_date`, within
/// a week's tolerance. Best-effort: returns `None` rather than erroring if
/// nothing is found, since the journal must work even with no log history.
pub fn find_model_composite(
    log_dir: &std::path::Path,
    ticker: &str,
    entry_date: NaiveDate,
) -> Option<f64> {
    let entries = crate::forward_test::read_all(log_dir).ok()?;
    let ticker = ticker.to_uppercase();
    entries
        .iter()
        .filter(|e| e.as_of <= entry_date && (entry_date - e.as_of).num_days() <= 7)
        .max_by_key(|e| e.as_of)
        .and_then(|e| e.ranking.iter().find(|r| r.ticker.to_uppercase() == ticker))
        .map(|r| r.composite)
}

pub fn add(cache: &Cache, decision: &NewDecision) -> Result<i64> {
    anyhow::ensure!(!decision.ticker.trim().is_empty(), "ticker must not be empty");
    anyhow::ensure!(!decision.thesis.trim().is_empty(), "a thesis is the whole point of a decision journal - it can't be empty");
    anyhow::ensure!(decision.entry_price > 0.0, "entry_price must be positive");
    anyhow::ensure!(decision.expected_holding_days > 0, "expected_holding_days must be positive");
    cache.insert_decision(decision).context("failed to record the journal entry")
}

pub fn close(cache: &Cache, id: i64, exit_date: NaiveDate, exit_price: f64, notes: Option<&str>) -> Result<()> {
    anyhow::ensure!(exit_price > 0.0, "exit_price must be positive");
    cache.close_decision(id, exit_date, exit_price, notes)
}

pub fn list(cache: &Cache, status: Option<&str>) -> Result<Vec<JournalEntry>> {
    cache.list_decisions(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn entry(status: &str, entry_price: f64, exit_price: Option<f64>, model: Option<f64>) -> JournalEntry {
        JournalEntry {
            id: 1,
            ticker: "X".into(),
            entry_date: d("2024-01-01"),
            thesis: "test".into(),
            expected_holding_days: 20,
            entry_price,
            model_composite_at_entry: model,
            status: status.into(),
            exit_date: exit_price.map(|_| d("2024-01-25")),
            exit_price,
            outcome_notes: None,
        }
    }

    #[test]
    fn is_due_only_once_the_holding_period_has_elapsed_and_still_open() {
        let e = entry("open", 100.0, None, None);
        assert!(!e.is_due(d("2024-01-10")));
        assert!(e.is_due(d("2024-01-21")));
        let closed = entry("closed", 100.0, Some(110.0), None);
        assert!(!closed.is_due(d("2024-02-01")), "a closed entry is never due");
    }

    #[test]
    fn days_held_never_goes_negative() {
        let e = entry("open", 100.0, None, None);
        assert_eq!(e.days_held(d("2023-12-25")), 0);
        assert_eq!(e.days_held(d("2024-01-11")), 10);
    }

    #[test]
    fn return_pct_is_none_while_open_and_correct_once_closed() {
        assert!(entry("open", 100.0, None, None).return_pct().is_none());
        let e = entry("closed", 100.0, Some(120.0), None);
        assert!((e.return_pct().unwrap() - 20.0).abs() < 1e-9);
        let loser = entry("closed", 100.0, Some(80.0), None);
        assert!((loser.return_pct().unwrap() + 20.0).abs() < 1e-9);
    }

    #[test]
    fn agreement_uses_the_60_threshold_and_is_none_without_a_model_score() {
        assert_eq!(entry("open", 100.0, None, Some(75.0)).agreed_with_model(), Some(true));
        assert_eq!(entry("open", 100.0, None, Some(45.0)).agreed_with_model(), Some(false));
        assert_eq!(entry("open", 100.0, None, Some(60.0)).agreed_with_model(), Some(true));
        assert_eq!(entry("open", 100.0, None, None).agreed_with_model(), None);
    }

    #[test]
    fn summarize_splits_by_agreement_and_ignores_open_entries() {
        let entries = vec![
            entry("closed", 100.0, Some(110.0), Some(80.0)), // agreed, +10%
            entry("closed", 100.0, Some(90.0), Some(80.0)),  // agreed, -10%
            entry("closed", 100.0, Some(130.0), Some(20.0)), // disagreed, +30%
            entry("open", 100.0, None, Some(80.0)),          // ignored (open)
        ];
        let s = summarize(&entries);
        assert_eq!(s.n_closed, 3);
        assert!((s.mean_return_pct - 10.0).abs() < 1e-6, "{}", s.mean_return_pct); // (10-10+30)/3
        assert!((s.hit_rate_pct - 66.666).abs() < 0.01);
        assert_eq!(s.n_agreed_with_model, 2);
        assert_eq!(s.n_disagreed_with_model, 1);
        assert!((s.mean_return_when_agreed_pct - 0.0).abs() < 1e-6); // (10-10)/2
        assert!((s.mean_return_when_disagreed_pct - 30.0).abs() < 1e-6);
    }

    #[test]
    fn summarize_of_no_closed_entries_is_the_zero_default() {
        let s = summarize(&[entry("open", 100.0, None, None)]);
        assert_eq!(s.n_closed, 0);
        assert_eq!(s.mean_return_pct, 0.0);
    }

    #[test]
    fn add_rejects_an_empty_thesis_or_nonsense_prices() {
        let cache = Cache::open(":memory:").unwrap();
        let base = NewDecision {
            ticker: "AAPL".into(), entry_date: d("2024-01-01"), thesis: "solid quarter ahead".into(),
            expected_holding_days: 20, entry_price: 150.0, model_composite_at_entry: None,
        };
        assert!(add(&cache, &NewDecision { thesis: "".into(), ..base.clone() }).is_err());
        assert!(add(&cache, &NewDecision { entry_price: 0.0, ..base.clone() }).is_err());
        assert!(add(&cache, &NewDecision { expected_holding_days: 0, ..base.clone() }).is_err());
        assert!(add(&cache, &base).is_ok());
    }

    #[test]
    fn full_lifecycle_add_list_close_list() {
        let cache = Cache::open(":memory:").unwrap();
        let id = add(&cache, &NewDecision {
            ticker: "AAPL".into(), entry_date: d("2024-01-01"),
            thesis: "iPhone cycle reacceleration".into(), expected_holding_days: 20,
            entry_price: 150.0, model_composite_at_entry: Some(72.0),
        }).unwrap();

        let open = list(&cache, Some("open")).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, id);
        assert!(open[0].is_open());

        close(&cache, id, d("2024-01-25"), 165.0, Some("thesis played out")).unwrap();

        assert!(list(&cache, Some("open")).unwrap().is_empty());
        let closed = list(&cache, Some("closed")).unwrap();
        assert_eq!(closed.len(), 1);
        assert!((closed[0].return_pct().unwrap() - 10.0).abs() < 1e-6);
        assert_eq!(closed[0].outcome_notes.as_deref(), Some("thesis played out"));

        assert_eq!(list(&cache, None).unwrap().len(), 1);
    }

    #[test]
    fn closing_an_already_closed_or_missing_id_fails_cleanly() {
        let cache = Cache::open(":memory:").unwrap();
        let id = add(&cache, &NewDecision {
            ticker: "X".into(), entry_date: d("2024-01-01"), thesis: "t".into(),
            expected_holding_days: 5, entry_price: 10.0, model_composite_at_entry: None,
        }).unwrap();
        close(&cache, id, d("2024-01-10"), 12.0, None).unwrap();
        assert!(close(&cache, id, d("2024-01-11"), 13.0, None).is_err(), "double close must fail");
        assert!(close(&cache, id + 999, d("2024-01-11"), 13.0, None).is_err());
    }

    #[test]
    fn find_model_composite_matches_within_a_week_and_prefers_the_latest() {
        let dir = std::env::temp_dir().join(format!(
            "qe_journal_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let scores = |composite: f64| {
            crate::signals::composite_score(
                "AAPL", "Tech", composite / 50.0 - 1.0, 0.0, 0.0, 0.0, 0.0, true,
                &crate::signals::SignalWeights { momentum: 1.0, fundamental: 0.0, insider: 0.0, sentiment: 0.0, pairs: 0.0 },
                &crate::signals::SignalAvailability { momentum: true, ..Default::default() },
            )
        };
        crate::forward_test::record(&dir, &crate::forward_test::LogInput {
            as_of: d("2024-01-01"), data_through: d("2023-12-29"), strategy: "t",
            scores: &[scores(70.0)], picks: &[], vix: None,
        }).unwrap();
        crate::forward_test::record(&dir, &crate::forward_test::LogInput {
            as_of: d("2024-01-05"), data_through: d("2024-01-04"), strategy: "t",
            scores: &[scores(80.0)], picks: &[], vix: None,
        }).unwrap();

        // Entry on 1/6: closest prior entry within a week is 1/5 (80.0), not 1/1.
        let found = find_model_composite(&dir, "aapl", d("2024-01-06")).unwrap();
        assert!((found - 80.0).abs() < 1e-6, "{found}");

        // Entry more than a week after any logged date: no match.
        assert!(find_model_composite(&dir, "AAPL", d("2024-01-20")).is_none());
        // Unknown ticker: no match.
        assert!(find_model_composite(&dir, "ZZZZ", d("2024-01-06")).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }
}
