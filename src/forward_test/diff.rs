//! "What changed since yesterday": a diff between two forward-log entries.
//!
//! Reading the full ranking every morning is slow going; most days only a
//! handful of things actually move. This surfaces just the movement: names
//! newly picked or dropped, a regime flip, and which held names moved the
//! most in score.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::ForwardEntry;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositeChange {
    pub ticker: String,
    pub prev_composite: f64,
    pub curr_composite: f64,
    pub delta: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryDiff {
    pub prev_date: chrono::NaiveDate,
    pub curr_date: chrono::NaiveDate,
    /// Newly recommended (in curr.picks, not in prev.picks).
    pub picks_added: Vec<String>,
    /// No longer recommended (in prev.picks, not in curr.picks).
    pub picks_removed: Vec<String>,
    /// `Some((prev, curr))` when the macro regime flipped.
    pub regime_changed: Option<(bool, bool)>,
    /// Composite-score movers among names ranked in both entries, sorted by
    /// |delta| descending.
    pub biggest_movers: Vec<CompositeChange>,
    pub prev_universe_size: usize,
    pub curr_universe_size: usize,
}

/// Diff two entries. Order doesn't matter for correctness — `prev` is
/// whichever has the earlier `as_of`.
pub fn diff(a: &ForwardEntry, b: &ForwardEntry) -> EntryDiff {
    let (prev, curr) = if a.as_of <= b.as_of { (a, b) } else { (b, a) };

    let prev_picks: std::collections::HashSet<&str> = prev.picks.iter().map(|p| p.ticker.as_str()).collect();
    let curr_picks: std::collections::HashSet<&str> = curr.picks.iter().map(|p| p.ticker.as_str()).collect();

    let mut picks_added: Vec<String> = curr_picks.difference(&prev_picks).map(|s| s.to_string()).collect();
    let mut picks_removed: Vec<String> = prev_picks.difference(&curr_picks).map(|s| s.to_string()).collect();
    picks_added.sort();
    picks_removed.sort();

    let prev_scores: HashMap<&str, f64> = prev.ranking.iter().map(|r| (r.ticker.as_str(), r.composite)).collect();
    let mut biggest_movers: Vec<CompositeChange> = curr
        .ranking
        .iter()
        .filter_map(|r| {
            prev_scores.get(r.ticker.as_str()).map(|&p| CompositeChange {
                ticker: r.ticker.clone(),
                prev_composite: p,
                curr_composite: r.composite,
                delta: r.composite - p,
            })
        })
        .collect();
    biggest_movers.sort_by(|a, b| b.delta.abs().partial_cmp(&a.delta.abs()).unwrap_or(std::cmp::Ordering::Equal));

    EntryDiff {
        prev_date: prev.as_of,
        curr_date: curr.as_of,
        picks_added,
        picks_removed,
        regime_changed: (prev.macro_on != curr.macro_on).then_some((prev.macro_on, curr.macro_on)),
        biggest_movers,
        prev_universe_size: prev.universe_size,
        curr_universe_size: curr.universe_size,
    }
}

/// Diff the two most recent entries in a log directory. `None` if there
/// aren't at least two.
pub fn diff_latest(dir: &std::path::Path) -> anyhow::Result<Option<EntryDiff>> {
    let entries = super::read_all(dir)?;
    if entries.len() < 2 {
        return Ok(None);
    }
    let n = entries.len();
    Ok(Some(diff(&entries[n - 2], &entries[n - 1])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forward_test::{record, LogInput};
    use crate::signals::{composite_score, SignalAvailability, SignalWeights};
    use crate::signals::SignalScore;
    use chrono::NaiveDate;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn score(t: &str, raw: f64, macro_on: bool) -> SignalScore {
        composite_score(
            t, "Ind", raw, 0.0, 0.0, 0.0, 0.0, macro_on,
            &SignalWeights { momentum: 1.0, fundamental: 0.0, insider: 0.0, sentiment: 0.0, pairs: 0.0 },
            &SignalAvailability { momentum: true, ..Default::default() },
        )
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "qe_diff_{tag}_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn detects_added_and_removed_picks() {
        let prev = ForwardEntry {
            schema_version: 1, as_of: d("2024-01-01"), data_through: d("2023-12-29"),
            generated_at_utc: String::new(), code_version: None, strategy: "t".into(),
            macro_on: true, vix: None, universe_size: 2, universe_hash: String::new(),
            picks: vec![score("A", 0.5, true), score("B", 0.5, true)].iter().map(|s| crate::forward_test::LoggedPick {
                ticker: s.ticker.clone(), rank: 1, composite: s.composite, momentum_raw: 0.0, fundamental_raw: 0.0,
                insider_raw: 0.0, sentiment_raw: 0.0, pairs_raw: 0.0, signals_available: 1, missing_signals: vec![],
            }).collect(),
            ranking: vec![], prev_hash: String::new(), hash: String::new(),
        };
        let mut curr = prev.clone();
        curr.as_of = d("2024-01-02");
        curr.picks.retain(|p| p.ticker != "B"); // B dropped
        curr.picks.push(crate::forward_test::LoggedPick {
            ticker: "C".into(), rank: 2, composite: 60.0, momentum_raw: 0.0, fundamental_raw: 0.0,
            insider_raw: 0.0, sentiment_raw: 0.0, pairs_raw: 0.0, signals_available: 1, missing_signals: vec![],
        }); // C added

        let dd = diff(&prev, &curr);
        assert_eq!(dd.picks_added, vec!["C"]);
        assert_eq!(dd.picks_removed, vec!["B"]);
        assert!(dd.regime_changed.is_none());
    }

    #[test]
    fn detects_a_regime_flip() {
        let mut prev = ForwardEntry {
            schema_version: 1, as_of: d("2024-01-01"), data_through: d("2023-12-29"),
            generated_at_utc: String::new(), code_version: None, strategy: "t".into(),
            macro_on: true, vix: None, universe_size: 0, universe_hash: String::new(),
            picks: vec![], ranking: vec![], prev_hash: String::new(), hash: String::new(),
        };
        let mut curr = prev.clone();
        curr.as_of = d("2024-01-02");
        curr.macro_on = false;
        assert_eq!(diff(&prev, &curr).regime_changed, Some((true, false)));
        prev.macro_on = false;
        curr.macro_on = false;
        assert!(diff(&prev, &curr).regime_changed.is_none());
    }

    #[test]
    fn ranks_movers_by_absolute_delta_and_ignores_names_not_in_both() {
        let mk_ranking = |pairs: &[(&str, f64)]| pairs.iter().map(|(t, c)| crate::forward_test::RankedName {
            ticker: t.to_string(), composite: *c, signals_available: 1,
        }).collect::<Vec<_>>();

        let prev = ForwardEntry {
            schema_version: 1, as_of: d("2024-01-01"), data_through: d("2023-12-29"),
            generated_at_utc: String::new(), code_version: None, strategy: "t".into(),
            macro_on: true, vix: None, universe_size: 3, universe_hash: String::new(),
            picks: vec![], ranking: mk_ranking(&[("A", 50.0), ("B", 50.0), ("ONLY_PREV", 90.0)]),
            prev_hash: String::new(), hash: String::new(),
        };
        let mut curr = prev.clone();
        curr.as_of = d("2024-01-02");
        curr.ranking = mk_ranking(&[("A", 55.0), ("B", 30.0), ("ONLY_CURR", 10.0)]); // A +5, B -20

        let dd = diff(&prev, &curr);
        assert_eq!(dd.biggest_movers.len(), 2, "ONLY_PREV/ONLY_CURR must be excluded");
        assert_eq!(dd.biggest_movers[0].ticker, "B", "the bigger move (|-20|) ranks first");
        assert!((dd.biggest_movers[0].delta + 20.0).abs() < 1e-9);
        assert!((dd.biggest_movers[1].delta - 5.0).abs() < 1e-9);
    }

    #[test]
    fn order_of_arguments_does_not_matter() {
        let prev = ForwardEntry {
            schema_version: 1, as_of: d("2024-01-01"), data_through: d("2023-12-29"),
            generated_at_utc: String::new(), code_version: None, strategy: "t".into(),
            macro_on: true, vix: None, universe_size: 0, universe_hash: String::new(),
            picks: vec![], ranking: vec![], prev_hash: String::new(), hash: String::new(),
        };
        let mut curr = prev.clone();
        curr.as_of = d("2024-01-05");
        curr.macro_on = false;
        let d1 = diff(&prev, &curr);
        let d2 = diff(&curr, &prev);
        assert_eq!(d1.prev_date, d2.prev_date);
        assert_eq!(d1.curr_date, d2.curr_date);
        assert_eq!(d1.regime_changed, d2.regime_changed);
    }

    #[test]
    fn diff_latest_needs_at_least_two_real_entries_on_disk() {
        let dir = tmp_dir("latest");
        assert!(diff_latest(&dir).unwrap().is_none());

        let scores = vec![score("A", 0.6, true)];
        record(&dir, &LogInput { as_of: d("2024-03-01"), data_through: d("2024-02-29"), strategy: "t", scores: &scores, picks: &scores, vix: None }).unwrap();
        assert!(diff_latest(&dir).unwrap().is_none(), "still only one entry");

        let scores2 = vec![score("A", -0.6, true)];
        record(&dir, &LogInput { as_of: d("2024-03-04"), data_through: d("2024-03-01"), strategy: "t", scores: &scores2, picks: &[], vix: None }).unwrap();
        let dd = diff_latest(&dir).unwrap().unwrap();
        assert_eq!(dd.prev_date, d("2024-03-01"));
        assert_eq!(dd.curr_date, d("2024-03-04"));
        assert_eq!(dd.picks_removed, vec!["A"]);

        std::fs::remove_dir_all(&dir).ok();
    }
}
