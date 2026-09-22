//! Forward-test log: a tamper-evident record of what the model recommended,
//! written *before* the outcome is known.
//!
//! A backtest can always be tuned until it looks good. A forward log cannot: the
//! picks for day D are written on day D and committed to git, so their
//! timestamp is external evidence that they were not chosen with hindsight.
//! After enough weeks, `--forward-eval` scores them against the market — the
//! only out-of-sample result that is honest by construction.
//!
//! Integrity:
//!   * one immutable file per as-of date (`forward_log/YYYY-MM-DD.json`);
//!     recording refuses to overwrite or to back-date;
//!   * every entry stores the SHA-256 of the previous one, so editing or
//!     deleting any earlier entry breaks the chain (`--forward-verify`);
//!   * the code version and universe hash are stored with each entry;
//!   * committing the directory to git timestamps it externally. (A chain
//!     alone cannot detect deleting the *latest* entries — git can.)
//!
//! Evaluation is aligned with the backtester: a pick made from data through
//! day `t` is entered at the **next bar's open**.

use anyhow::{anyhow, bail, Context, Result};
use chrono::{Duration, NaiveDate};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use crate::data::asof::AsOf;
use crate::data::cache::Cache;
use crate::data::prices::{PriceSeries, PriceStore};
use crate::data::yahoo::YahooFinance;
use crate::data::DataSource;
use crate::metrics::ic::{self, IcSummary};
use crate::signals::SignalScore;

pub mod diff;

pub const SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_DIR: &str = "forward_log";

// ── Entry format ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoggedPick {
    pub ticker: String,
    pub rank: usize,
    pub composite: f64,
    pub momentum_raw: f64,
    pub fundamental_raw: f64,
    pub insider_raw: f64,
    pub sentiment_raw: f64,
    pub pairs_raw: f64,
    pub signals_available: usize,
    pub missing_signals: Vec<String>,
}

impl LoggedPick {
    fn from_score(s: &SignalScore) -> Self {
        Self {
            ticker: s.ticker.clone(),
            rank: s.rank,
            composite: round_stable(s.composite),
            momentum_raw: round_stable(s.momentum_raw),
            fundamental_raw: round_stable(s.fundamental_raw),
            insider_raw: round_stable(s.insider_raw),
            sentiment_raw: round_stable(s.sentiment_raw),
            pairs_raw: round_stable(s.pairs_raw),
            signals_available: s.signals_available(),
            missing_signals: s.availability.missing().into_iter().map(String::from).collect(),
        }
    }
}

/// Compact record of every scored name, so any alternative top-N can be
/// evaluated later without re-running the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RankedName {
    pub ticker: String,
    pub composite: f64,
    pub signals_available: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForwardEntry {
    pub schema_version: u32,
    /// The run date (the date the signals were computed for).
    pub as_of: NaiveDate,
    /// Latest price bar the signals could see. Fills are evaluated from the
    /// first bar strictly after this, so it stays correct whether the job runs
    /// before the open or after the close.
    pub data_through: NaiveDate,
    pub generated_at_utc: String,
    /// `git` commit of the code that produced this (with `-dirty` if modified).
    pub code_version: Option<String>,
    pub strategy: String,
    pub macro_on: bool,
    pub vix: Option<f64>,
    pub universe_size: usize,
    pub universe_hash: String,
    /// Recommended holdings. Empty means "hold cash" (risk-off gate).
    pub picks: Vec<LoggedPick>,
    pub ranking: Vec<RankedName>,
    /// SHA-256 of the previous entry (empty for the first).
    pub prev_hash: String,
    /// SHA-256 over this entry with `hash` blanked.
    pub hash: String,
}

/// Round to 9 significant decimal digits.
///
/// The hash chain must be exactly reproducible from a re-parsed JSON file, but
/// serde_json 1.0's number parser is not always correctly-rounded: it can
/// return an f64 one ULP away from the value its OWN serializer wrote (Rust's
/// std `f64::from_str` parses the identical text exactly; serde_json's
/// `from_str` does not, verified directly against a real backfill entry -
/// see docs/AUDIT.md). Composite/signal scores carry no meaningful precision
/// past a handful of decimal digits anyway, so rounding before it ever
/// reaches JSON sidesteps the bug entirely rather than depending on a
/// third-party parser's bit-exactness.
fn round_stable(v: f64) -> f64 {
    if !v.is_finite() {
        return v;
    }
    let magnitude = if v == 0.0 { 0.0 } else { v.abs().log10().floor() };
    let decimals = (8.0 - magnitude).clamp(0.0, 12.0);
    let scale = 10f64.powf(decimals);
    (v * scale).round() / scale
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

impl ForwardEntry {
    fn compute_hash(&self) -> String {
        let mut e = self.clone();
        e.hash = String::new();
        // serde emits struct fields in declaration order: deterministic.
        sha256_hex(&serde_json::to_vec(&e).unwrap_or_default())
    }
}

/// SHA-256 over the sorted, de-duplicated ticker list.
pub fn universe_hash(tickers: &[String]) -> String {
    let mut t: Vec<&str> = tickers.iter().map(String::as_str).collect();
    t.sort_unstable();
    t.dedup();
    sha256_hex(t.join("\n").as_bytes())
}

// ── Recording ─────────────────────────────────────────────────────────────────

pub struct LogInput<'a> {
    pub as_of: NaiveDate,
    pub data_through: NaiveDate,
    pub strategy: &'a str,
    /// Full ranking (all scored names).
    pub scores: &'a [SignalScore],
    /// What the model recommends holding (may be empty = cash).
    pub picks: &'a [SignalScore],
    pub vix: Option<f64>,
}

fn entry_path(dir: &Path, date: NaiveDate) -> PathBuf {
    dir.join(format!("{date}.json"))
}

/// All entries in `dir`, oldest first. Files that don't look like entries are ignored.
pub fn read_all(dir: &Path) -> Result<Vec<ForwardEntry>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension().map_or(false, |x| x == "json")
                && p.file_stem()
                    .and_then(|s| s.to_str())
                    .map_or(false, |s| s.parse::<NaiveDate>().is_ok())
        })
        .collect();
    files.sort();
    files
        .iter()
        .map(|p| {
            let text = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
            serde_json::from_str(&text).with_context(|| format!("parsing {}", p.display()))
        })
        .collect()
}

/// Append one entry. Refuses to overwrite an existing date or to add an entry
/// that is not newer than the latest, so history cannot be rewritten through
/// this API.
pub fn record(dir: &Path, input: &LogInput) -> Result<ForwardEntry> {
    std::fs::create_dir_all(dir)?;
    let existing = read_all(dir)?;

    if let Some(last) = existing.last() {
        if input.as_of <= last.as_of {
            bail!(
                "forward log is append-only: {} is not newer than the latest entry ({}). \
                 An entry for that date already exists or would be back-dated.",
                input.as_of, last.as_of
            );
        }
    }

    let tickers: Vec<String> = input.scores.iter().map(|s| s.ticker.clone()).collect();
    let mut entry = ForwardEntry {
        schema_version: SCHEMA_VERSION,
        as_of: input.as_of,
        data_through: input.data_through,
        generated_at_utc: chrono::Utc::now().to_rfc3339(),
        code_version: code_version(),
        strategy: input.strategy.to_string(),
        macro_on: input.scores.first().map_or(true, |s| s.macro_on),
        vix: input.vix.map(round_stable),
        universe_size: input.scores.len(),
        universe_hash: universe_hash(&tickers),
        picks: input.picks.iter().map(LoggedPick::from_score).collect(),
        ranking: input
            .scores
            .iter()
            .map(|s| RankedName {
                ticker: s.ticker.clone(),
                composite: round_stable(s.composite),
                signals_available: s.signals_available(),
            })
            .collect(),
        prev_hash: existing.last().map(|e| e.hash.clone()).unwrap_or_default(),
        hash: String::new(),
    };
    entry.hash = entry.compute_hash();

    let path = entry_path(dir, entry.as_of);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&entry)?)?;
    std::fs::rename(&tmp, &path)?;
    info!(as_of = %entry.as_of, picks = entry.picks.len(), hash = %&entry.hash[..12], "forward-test entry recorded");
    Ok(entry)
}

/// Record today's run, but never let logging break the daily job.
pub fn record_best_effort(
    cache: &Cache,
    as_of: NaiveDate,
    strategy: &str,
    scores: &[SignalScore],
    picks: &[SignalScore],
) {
    if scores.is_empty() {
        return;
    }
    let dir = std::env::var("FORWARD_LOG_DIR").unwrap_or_else(|_| DEFAULT_DIR.to_string());
    let data_through = data_through(cache, scores, as_of);
    let vix = None; // regime flag is recorded; the level is informational only
    let input = LogInput { as_of, data_through, strategy, scores, picks, vix };
    match record(Path::new(&dir), &input) {
        Ok(_) => {}
        Err(e) => warn!("forward-test log not written: {e:#}"),
    }
}

/// Latest price bar (on/before `as_of`) any scored name had: what the signals could see.
pub fn data_through(cache: &Cache, scores: &[SignalScore], as_of: NaiveDate) -> NaiveDate {
    let view = AsOf::new(cache, as_of);
    scores
        .iter()
        .filter_map(|s| view.last_bar(&s.ticker).map(|b| b.date))
        .max()
        .unwrap_or(as_of)
}

fn code_version() -> Option<String> {
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let head = run(&["rev-parse", "--short", "HEAD"])?;
    let dirty = run(&["status", "--porcelain", "--untracked-files=no"]).map_or(false, |s| !s.is_empty());
    Some(if dirty { format!("{head}-dirty") } else { head })
}

// ── Verification ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct VerifyReport {
    pub n_entries: usize,
    pub first_error: Option<String>,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.first_error.is_none()
    }
}

/// Recompute every hash and check each entry links to the one before it.
pub fn verify(dir: &Path) -> Result<VerifyReport> {
    let entries = read_all(dir)?;
    let mut prev: Option<&ForwardEntry> = None;
    for e in &entries {
        let fail = |msg: String| {
            Ok(VerifyReport { n_entries: entries.len(), first_error: Some(msg) })
        };
        if e.compute_hash() != e.hash {
            return fail(format!("{}: contents do not match their hash (edited after the fact)", e.as_of));
        }
        let expected_prev = prev.map(|p| p.hash.as_str()).unwrap_or("");
        if e.prev_hash != expected_prev {
            return fail(format!("{}: does not link to the previous entry (an entry was removed or replaced)", e.as_of));
        }
        if let Some(p) = prev {
            if e.as_of <= p.as_of {
                return fail(format!("{}: dates are not strictly increasing", e.as_of));
            }
        }
        prev = Some(e);
    }
    Ok(VerifyReport { n_entries: entries.len(), first_error: None })
}

// ── Evaluation ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub as_of: NaiveDate,
    pub entry_date: NaiveDate,
    pub exit_date: NaiveDate,
    pub n_picks: usize,
    /// True when the model recommended cash (basket return is 0).
    pub in_cash: bool,
    pub basket_return: f64,
    pub benchmark_return: Option<f64>,
    /// `basket_return - benchmark_return`.
    pub excess: Option<f64>,
    /// Cross-sectional rank IC of the WHOLE logged ranking vs. forward return
    /// (needs >= 20 names with matured prices). Far less noisy than the top-N basket.
    pub rank_ic: Option<f64>,
}

/// Return of buying at the open of the first bar after `after` and selling at
/// the close of the `horizon`-th bar (the entry bar counts as bar 1).
/// `None` if the series does not extend that far yet ("not matured").
fn hold_return(series: &PriceSeries, after: NaiveDate, horizon: usize) -> Option<(NaiveDate, NaiveDate, f64)> {
    let bars = series.bars();
    let i = bars.partition_point(|b| b.date <= after);
    let entry = bars.get(i)?;
    let exit = bars.get(i + horizon.checked_sub(1)?)?;
    let px = entry.adj_open();
    (px.is_finite() && px > 0.0).then(|| (entry.date, exit.date, exit.adj_close / px - 1.0))
}

/// Score one logged entry over `horizon` trading days. Returns `None` until
/// every pick has that much history.
pub fn evaluate_entry(
    entry: &ForwardEntry,
    prices: &PriceStore,
    benchmark: Option<&PriceSeries>,
    horizon: usize,
) -> Option<Outcome> {
    let bench = benchmark.and_then(|b| hold_return(b, entry.data_through, horizon));

    let (mut xs, mut ys) = (Vec::new(), Vec::new());
    for r in &entry.ranking {
        if let Some((_, _, ret)) = prices.get(&r.ticker).and_then(|s| hold_return(s, entry.data_through, horizon)) {
            xs.push(r.composite);
            ys.push(ret);
        }
    }
    let rank_ic = if xs.len() >= 20 { ic::spearman(&xs, &ys) } else { None };

    if entry.picks.is_empty() {
        // Cash: return 0, dated by the benchmark's calendar.
        let (entry_date, exit_date, b) = bench?;
        return Some(Outcome {
            as_of: entry.as_of, entry_date, exit_date, n_picks: 0, in_cash: true,
            basket_return: 0.0, benchmark_return: Some(b), excess: Some(-b), rank_ic,
        });
    }

    let mut rets = Vec::new();
    let mut dates: Option<(NaiveDate, NaiveDate)> = None;
    for p in &entry.picks {
        let Some(series) = prices.get(&p.ticker) else { continue };
        // A held name that hasn't matured means the whole entry hasn't.
        let (d0, d1, r) = hold_return(series, entry.data_through, horizon)?;
        dates.get_or_insert((d0, d1));
        rets.push(r);
    }
    if rets.is_empty() {
        return None;
    }
    let basket = rets.iter().sum::<f64>() / rets.len() as f64;
    let (entry_date, exit_date) = dates?;
    Some(Outcome {
        as_of: entry.as_of,
        entry_date,
        exit_date,
        n_picks: rets.len(),
        in_cash: false,
        basket_return: basket,
        benchmark_return: bench.map(|b| b.2),
        excess: bench.map(|b| basket - b.2),
        rank_ic,
    })
}

// ── Mark-to-market status (open calls, before the horizon matures) ─────────────

/// Return from the open of the first bar after `after` to the CLOSE OF THE
/// LATEST AVAILABLE BAR, whatever that is — unlike `hold_return`, this never
/// requires a fixed horizon to have elapsed. `None` if the pick hasn't even
/// entered yet (no bar after `after`) or nothing has traded since entry.
fn mark_to_market(series: &PriceSeries, after: NaiveDate) -> Option<(NaiveDate, NaiveDate, f64, usize)> {
    let bars = series.bars();
    let i = bars.partition_point(|b| b.date <= after);
    let entry = bars.get(i)?;
    let latest = bars.last()?;
    if latest.date <= entry.date {
        return None;
    }
    let px = entry.adj_open();
    (px.is_finite() && px > 0.0).then(|| (entry.date, latest.date, latest.adj_close / px - 1.0, bars.len() - i))
}

#[derive(Debug, Clone, PartialEq)]
pub struct PickStatus {
    pub ticker: String,
    pub entry_date: NaiveDate,
    pub as_of: NaiveDate,
    pub return_so_far: f64,
    pub trading_days_held: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EntryStatus {
    pub logged_on: NaiveDate,
    pub in_cash: bool,
    pub picks: Vec<PickStatus>,
    pub basket_return_so_far: Option<f64>,
    pub benchmark_return_so_far: Option<f64>,
    /// True once every pick has at least `horizon` trading days behind it —
    /// `--forward-eval` will score this entry too, so it stops appearing here.
    pub matured: bool,
}

/// Mark-to-market view of one logged entry, using whatever price history
/// exists today — no need to wait for the full `--horizon` to elapse. This is
/// how you check on OPEN calls; `evaluate_entry` is the honest final grade
/// once they've fully played out.
pub fn status_entry(entry: &ForwardEntry, prices: &PriceStore, benchmark: Option<&PriceSeries>, horizon: usize) -> EntryStatus {
    let bench_mtm = benchmark.and_then(|b| mark_to_market(b, entry.data_through));

    if entry.picks.is_empty() {
        return EntryStatus {
            logged_on: entry.as_of,
            in_cash: true,
            picks: Vec::new(),
            basket_return_so_far: Some(0.0),
            benchmark_return_so_far: bench_mtm.map(|b| b.2),
            matured: true, // cash has nothing left to wait on
        };
    }

    let mut picks = Vec::new();
    for p in &entry.picks {
        let Some(series) = prices.get(&p.ticker) else { continue };
        if let Some((entry_date, as_of, ret, days_held)) = mark_to_market(series, entry.data_through) {
            picks.push(PickStatus { ticker: p.ticker.clone(), entry_date, as_of, return_so_far: ret, trading_days_held: days_held });
        }
    }
    let matured = !picks.is_empty() && picks.iter().all(|p| p.trading_days_held >= horizon);
    let basket = (!picks.is_empty())
        .then(|| picks.iter().map(|p| p.return_so_far).sum::<f64>() / picks.len() as f64);
    EntryStatus {
        logged_on: entry.as_of,
        in_cash: false,
        picks,
        basket_return_so_far: basket,
        benchmark_return_so_far: bench_mtm.map(|b| b.2),
        matured,
    }
}

/// Mark-to-market status of every logged entry that has NOT yet fully matured
/// at `horizon` (matured ones are `--forward-eval`'s job). Loads the same
/// price history `evaluate` does.
pub async fn status(dir: &Path, cache: &Cache, horizon: usize) -> Result<Vec<EntryStatus>> {
    let entries = read_all(dir)?;
    if entries.is_empty() {
        return Err(anyhow!("no forward-test entries in {}", dir.display()));
    }
    let today = chrono::Local::now().date_naive();
    let earliest = entries.iter().map(|e| e.data_through).min().unwrap_or(today);
    let yahoo = YahooFinance::new(cache.clone());

    let mut tickers: Vec<String> = entries.iter().flat_map(|e| e.picks.iter().map(|p| p.ticker.clone())).collect();
    tickers.sort();
    tickers.dedup();

    let mut prices = PriceStore::new();
    for t in &tickers {
        let bars = yahoo.price_history(t, earliest - Duration::days(5), today).await.unwrap_or_default();
        prices.insert(t.clone(), PriceSeries::new(bars));
    }

    let nse = tickers.iter().filter(|t| t.ends_with(".NS")).count();
    let bench_ticker = if nse * 2 > tickers.len() { "^NSEI" } else { "^GSPC" };
    let bench = yahoo.price_history(bench_ticker, earliest - Duration::days(5), today).await.ok().map(PriceSeries::new);

    Ok(entries
        .iter()
        .map(|e| status_entry(e, &prices, bench.as_ref(), horizon))
        .filter(|s| !s.matured)
        .collect())
}

#[derive(Debug, Clone, Default)]
pub struct EvalSummary {
    pub n: usize,
    pub mean_basket: f64,
    pub mean_excess: f64,
    /// Share of matured entries that beat the benchmark.
    pub hit_rate: f64,
    pub excess_t_stat: f64,
    pub cash_entries: usize,
    /// Mean cross-sectional rank IC over entries that have one.
    pub mean_ic: f64,
    pub ic_t_stat: f64,
    pub ic_n: usize,
}

pub fn summarize(outcomes: &[Outcome]) -> EvalSummary {
    let excess: Vec<f64> = outcomes.iter().filter_map(|o| o.excess).collect();
    let s: IcSummary = ic::summarize(&excess); // mean / std / t-stat of a series
    let ics: Vec<f64> = outcomes.iter().filter_map(|o| o.rank_ic).collect();
    let ic_s: IcSummary = ic::summarize(&ics);
    EvalSummary {
        n: outcomes.len(),
        mean_basket: if outcomes.is_empty() {
            0.0
        } else {
            outcomes.iter().map(|o| o.basket_return).sum::<f64>() / outcomes.len() as f64
        },
        mean_excess: s.mean,
        hit_rate: s.hit_rate,
        excess_t_stat: s.t_stat,
        cash_entries: outcomes.iter().filter(|o| o.in_cash).count(),
        mean_ic: ic_s.mean,
        ic_t_stat: ic_s.t_stat,
        ic_n: ic_s.n,
    }
}

/// Load prices and score every logged entry that has matured.
pub async fn evaluate(
    dir: &Path,
    cache: &Cache,
    horizon: usize,
) -> Result<(Vec<Outcome>, EvalSummary, usize)> {
    let entries = read_all(dir)?;
    if entries.is_empty() {
        return Err(anyhow!("no forward-test entries in {}", dir.display()));
    }
    let today = chrono::Local::now().date_naive();
    let earliest = entries.iter().map(|e| e.data_through).min().unwrap_or(today);
    let yahoo = YahooFinance::new(cache.clone());

    let mut tickers: Vec<String> = entries
        .iter()
        .flat_map(|e| e.picks.iter().map(|p| p.ticker.clone()).chain(e.ranking.iter().map(|r| r.ticker.clone())))
        .collect();
    tickers.sort();
    tickers.dedup();

    let mut prices = PriceStore::new();
    for t in &tickers {
        let bars = yahoo
            .price_history(t, earliest - Duration::days(5), today)
            .await
            .unwrap_or_default();
        prices.insert(t.clone(), PriceSeries::new(bars));
    }

    let nse = tickers.iter().filter(|t| t.ends_with(".NS")).count();
    let bench_ticker = if nse * 2 > tickers.len() { "^NSEI" } else { "^GSPC" };
    let bench = yahoo
        .price_history(bench_ticker, earliest - Duration::days(5), today)
        .await
        .ok()
        .map(PriceSeries::new);

    let outcomes: Vec<Outcome> = entries
        .iter()
        .filter_map(|e| evaluate_entry(e, &prices, bench.as_ref(), horizon))
        .collect();
    let pending = entries.len() - outcomes.len();
    let summary = summarize(&outcomes);
    Ok((outcomes, summary, pending))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::prices::test_bar;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn score(t: &str, raw: f64, macro_on: bool) -> SignalScore {
        let mut s = crate::signals::composite_score(
            t, "Ind", raw, 0.0, 0.0, 0.0, 0.0, macro_on,
            &crate::signals::SignalWeights { momentum: 1.0, fundamental: 0.0, insider: 0.0, sentiment: 0.0, pairs: 0.0 },
            &crate::signals::SignalAvailability { momentum: true, ..Default::default() },
        );
        s.rank = 1;
        s
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "qe_fwd_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn rec(dir: &Path, date: &str, picks: &[SignalScore]) -> Result<ForwardEntry> {
        let scores = vec![score("AAA", 0.5, true), score("BBB", 0.1, true)];
        record(dir, &LogInput { as_of: d(date), data_through: d(date) - Duration::days(1), strategy: "test", scores: &scores, picks, vix: None })
    }

    #[test]
    fn hash_chain_survives_serde_jsons_float_parsing_imprecision() {
        // Regression: serde_json 1.0's deserializer is not always correctly
        // rounded. This exact bit pattern round-trips correctly through
        // Rust's own f64::from_str but comes back ONE ULP off through
        // serde_json::from_str - found via a real 30-day backfill run whose
        // very first entry failed --forward-verify. round_stable() must
        // remove that last-bit ambiguity before it ever reaches JSON.
        let treacherous = f64::from_bits(0x4027db518d26fdea);
        let text = serde_json::to_string(&treacherous).unwrap();
        let reparsed: f64 = serde_json::from_str(&text).unwrap();
        assert_ne!(
            reparsed.to_bits(), treacherous.to_bits(),
            "serde_json now round-trips this value exactly; round_stable()'s reasoning              may no longer be needed, but keep it (harmless) and update this test's comment"
        );
        assert_eq!(round_stable(reparsed), round_stable(treacherous));

        let dir = tmp_dir("float_precision");
        let scores = vec![score("A", 0.1, true), score("B", 0.837465219, true)];
        record(&dir, &LogInput {
            as_of: d("2024-03-04"), data_through: d("2024-03-01"), strategy: "x",
            scores: &scores, picks: &[], vix: Some(treacherous),
        }).unwrap();
        let v = verify(&dir).unwrap();
        assert!(v.ok(), "{v:?}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn round_stable_preserves_meaningful_precision_and_handles_edge_cases() {
        assert_eq!(round_stable(0.0), 0.0);
        assert!(round_stable(f64::NAN).is_nan());
        assert_eq!(round_stable(f64::INFINITY), f64::INFINITY);
        assert!((round_stable(0.123456789123) - 0.12345679).abs() < 1e-9, "{}", round_stable(0.123456789123));
        assert!((round_stable(75.0) - 75.0).abs() < 1e-9);
        assert!((round_stable(-0.837465219321) - -0.83746522).abs() < 1e-8, "{}", round_stable(-0.837465219321));
    }

    #[test]
    fn a_realistic_full_size_entry_verifies_reliably() {
        // 317 names is roughly a real S&P-500-sized ranking; non-round
        // composites are the norm, not the exception, for real signal output.
        let scores: Vec<SignalScore> = (0..317)
            .map(|i| {
                let raw = ((i as f64) * 0.0137 - 1.0).sin() * 0.913 + (i as f64).sqrt() * 1e-7;
                score(&format!("TICK{i:04}"), raw, true)
            })
            .collect();
        let picks: Vec<SignalScore> = scores.iter().take(30).cloned().collect();
        let dir = tmp_dir("full_size");
        record(&dir, &LogInput {
            as_of: d("2024-03-04"), data_through: d("2024-03-01"), strategy: "large",
            scores: &scores, picks: &picks, vix: Some(17.35),
        }).unwrap();
        let v = verify(&dir).unwrap();
        assert!(v.ok(), "{v:?}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_fresh_chain_verifies() {
        let dir = tmp_dir("ok");
        rec(&dir, "2024-03-01", &[score("AAA", 0.5, true)]).unwrap();
        rec(&dir, "2024-03-04", &[score("BBB", 0.4, true)]).unwrap();
        rec(&dir, "2024-03-05", &[]).unwrap();
        let v = verify(&dir).unwrap();
        assert!(v.ok(), "{:?}", v);
        assert_eq!(v.n_entries, 3);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn each_entry_commits_to_the_previous_one() {
        let dir = tmp_dir("link");
        let a = rec(&dir, "2024-03-01", &[score("AAA", 0.5, true)]).unwrap();
        let b = rec(&dir, "2024-03-04", &[score("AAA", 0.5, true)]).unwrap();
        assert_eq!(a.prev_hash, "");
        assert_eq!(b.prev_hash, a.hash);
        assert_ne!(a.hash, b.hash);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn editing_a_past_entry_is_detected() {
        let dir = tmp_dir("edit");
        rec(&dir, "2024-03-01", &[score("AAA", 0.5, true)]).unwrap();
        rec(&dir, "2024-03-04", &[score("BBB", 0.4, true)]).unwrap();

        // Rewrite history: pretend the first day's pick had a better score.
        let p = dir.join("2024-03-01.json");
        let text = std::fs::read_to_string(&p).unwrap().replace("\"AAA\"", "\"ZZZ\"");
        std::fs::write(&p, text).unwrap();

        let v = verify(&dir).unwrap();
        assert!(!v.ok());
        assert!(v.first_error.unwrap().contains("2024-03-01"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn deleting_a_middle_entry_is_detected() {
        let dir = tmp_dir("del");
        rec(&dir, "2024-03-01", &[score("AAA", 0.5, true)]).unwrap();
        rec(&dir, "2024-03-04", &[score("AAA", 0.5, true)]).unwrap();
        rec(&dir, "2024-03-05", &[score("AAA", 0.5, true)]).unwrap();
        std::fs::remove_file(dir.join("2024-03-04.json")).unwrap();
        let v = verify(&dir).unwrap();
        assert!(!v.ok());
        assert!(v.first_error.unwrap().contains("2024-03-05"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn recording_refuses_overwrites_and_back_dating() {
        let dir = tmp_dir("append");
        rec(&dir, "2024-03-04", &[score("AAA", 0.5, true)]).unwrap();
        assert!(rec(&dir, "2024-03-04", &[score("BBB", 0.5, true)]).is_err(), "same date: overwrite");
        assert!(rec(&dir, "2024-03-01", &[score("BBB", 0.5, true)]).is_err(), "earlier date: back-dating");
        assert_eq!(read_all(&dir).unwrap().len(), 1);
        assert!(rec(&dir, "2024-03-05", &[]).is_ok());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn an_empty_directory_verifies_trivially() {
        let dir = tmp_dir("empty");
        let v = verify(&dir).unwrap();
        assert!(v.ok() && v.n_entries == 0);
        std::fs::remove_dir_all(dir).ok();
        assert!(verify(Path::new("definitely/not/here")).unwrap().ok());
    }

    #[test]
    fn universe_hash_ignores_order_and_duplicates() {
        let a = universe_hash(&["B".into(), "A".into(), "A".into()]);
        let b = universe_hash(&["A".into(), "B".into()]);
        assert_eq!(a, b);
        assert_ne!(a, universe_hash(&["A".into(), "C".into()]));
    }

    #[test]
    fn risk_off_is_recorded_as_a_cash_entry() {
        let dir = tmp_dir("cash");
        let scores = vec![score("AAA", 0.5, false)];
        let e = record(&dir, &LogInput { as_of: d("2024-03-04"), data_through: d("2024-03-01"), strategy: "t", scores: &scores, picks: &[], vix: Some(31.0) }).unwrap();
        assert!(!e.macro_on);
        assert!(e.picks.is_empty());
        assert_eq!(e.vix, Some(31.0));
        std::fs::remove_dir_all(dir).ok();
    }

    // ── evaluation ────────────────────────────────────────────────────────────

    fn series_from(rows: &[(&str, f64, f64)]) -> PriceSeries {
        // (date, open, close)
        PriceSeries::new(
            rows.iter()
                .map(|(dt, o, c)| {
                    let mut b = test_bar(dt, *c);
                    b.open = *o;
                    b
                })
                .collect(),
        )
    }

    fn entry_with(picks: &[&str], data_through: &str) -> ForwardEntry {
        let scores: Vec<SignalScore> = picks.iter().map(|t| score(t, 0.5, true)).collect();
        let mut e = ForwardEntry {
            schema_version: 1,
            as_of: d(data_through) + Duration::days(1),
            data_through: d(data_through),
            generated_at_utc: String::new(),
            code_version: None,
            strategy: "t".into(),
            macro_on: true,
            vix: None,
            universe_size: 2,
            universe_hash: String::new(),
            picks: scores.iter().map(LoggedPick::from_score).collect(),
            ranking: vec![],
            prev_hash: String::new(),
            hash: String::new(),
        };
        e.hash = e.compute_hash();
        e
    }

    #[test]
    fn evaluation_enters_at_the_next_open_after_the_data_and_exits_at_the_horizon_close() {
        // Signals used data through Mon 3/4. Entry: Tue 3/5 open (100). Horizon 2:
        // exit = close of the 2nd bar from entry = Wed 3/6 close (120).
        let a = series_from(&[
            ("2024-03-04", 90.0, 90.0),
            ("2024-03-05", 100.0, 95.0),
            ("2024-03-06", 110.0, 120.0),
            ("2024-03-07", 120.0, 200.0),
        ]);
        let mut store = PriceStore::new();
        store.insert("AAA", a);
        let out = evaluate_entry(&entry_with(&["AAA"], "2024-03-04"), &store, None, 2).unwrap();
        assert_eq!(out.entry_date, d("2024-03-05"));
        assert_eq!(out.exit_date, d("2024-03-06"));
        assert!((out.basket_return - 0.20).abs() < 1e-9, "{}", out.basket_return);
        assert!(out.excess.is_none(), "no benchmark supplied");
    }

    #[test]
    fn an_entry_that_has_not_matured_is_pending_not_scored() {
        let a = series_from(&[("2024-03-04", 90.0, 90.0), ("2024-03-05", 100.0, 95.0)]);
        let mut store = PriceStore::new();
        store.insert("AAA", a);
        assert!(evaluate_entry(&entry_with(&["AAA"], "2024-03-04"), &store, None, 5).is_none());
    }

    #[test]
    fn basket_is_equal_weight_and_excess_is_vs_the_benchmark() {
        let up = series_from(&[("2024-03-04", 100.0, 100.0), ("2024-03-05", 100.0, 110.0), ("2024-03-06", 110.0, 120.0)]);
        let down = series_from(&[("2024-03-04", 100.0, 100.0), ("2024-03-05", 100.0, 95.0), ("2024-03-06", 95.0, 90.0)]);
        let bench = series_from(&[("2024-03-04", 100.0, 100.0), ("2024-03-05", 100.0, 101.0), ("2024-03-06", 101.0, 102.0)]);
        let mut store = PriceStore::new();
        store.insert("UP", up);
        store.insert("DOWN", down);
        let out = evaluate_entry(&entry_with(&["UP", "DOWN"], "2024-03-04"), &store, Some(&bench), 2).unwrap();
        // UP +20%, DOWN −10% → +5% basket; benchmark 100→102 = +2%.
        assert!((out.basket_return - 0.05).abs() < 1e-9);
        assert!((out.benchmark_return.unwrap() - 0.02).abs() < 1e-9);
        assert!((out.excess.unwrap() - 0.03).abs() < 1e-9);
    }

    #[test]
    fn a_cash_entry_returns_zero_and_loses_to_a_rising_benchmark() {
        let bench = series_from(&[("2024-03-04", 100.0, 100.0), ("2024-03-05", 100.0, 104.0), ("2024-03-06", 104.0, 110.0)]);
        let out = evaluate_entry(&entry_with(&[], "2024-03-04"), &PriceStore::new(), Some(&bench), 2).unwrap();
        assert!(out.in_cash);
        assert_eq!(out.basket_return, 0.0);
        assert!((out.excess.unwrap() + 0.10).abs() < 1e-9, "sat out a +10% rally");
        // Without a benchmark a cash entry cannot be dated at all.
        assert!(evaluate_entry(&entry_with(&[], "2024-03-04"), &PriceStore::new(), None, 2).is_none());
    }

    #[test]
    fn summary_counts_hits_and_cash() {
        let mk = |ex: f64, cash: bool| Outcome {
            as_of: d("2024-03-01"), entry_date: d("2024-03-04"), exit_date: d("2024-03-06"),
            n_picks: if cash { 0 } else { 3 }, in_cash: cash, basket_return: ex, benchmark_return: Some(0.0), excess: Some(ex),
            rank_ic: Some(if ex > 0.0 { 0.05 } else { -0.02 }),
        };
        let s = summarize(&[mk(0.02, false), mk(0.04, false), mk(-0.01, false), mk(0.0, true)]);
        assert_eq!(s.n, 4);
        assert_eq!(s.cash_entries, 1);
        assert!((s.hit_rate - 0.5).abs() < 1e-9);
        assert!((s.mean_excess - 0.0125).abs() < 1e-9);
        assert_eq!(summarize(&[]).n, 0);
        assert_eq!(s.ic_n, 4);
        assert!(s.mean_ic > 0.0);
    }

    #[test]
    fn rank_ic_uses_the_whole_ranking_not_just_the_picks() {
        // 25 names; composite order == forward-return order, so IC ~ +1 even
        // though only one name is a "pick".
        let mut store = PriceStore::new();
        let mut ranking = Vec::new();
        for i in 0..25 {
            let t = format!("T{i:02}");
            let end = 100.0 + i as f64; // higher composite -> higher return
            store.insert(t.clone(), series_from(&[("2024-03-04", 100.0, 100.0), ("2024-03-05", 100.0, 100.0), ("2024-03-06", 100.0, end)]));
            ranking.push(RankedName { ticker: t, composite: 40.0 + i as f64, signals_available: 3 });
        }
        let mut e = entry_with(&["T24"], "2024-03-04");
        e.ranking = ranking;
        let out = evaluate_entry(&e, &store, None, 2).unwrap();
        assert!(out.rank_ic.unwrap() > 0.99, "{:?}", out.rank_ic);

        // Too few names -> no IC rather than a noisy one.
        e.ranking.truncate(10);
        assert!(evaluate_entry(&e, &store, None, 2).unwrap().rank_ic.is_none());
    }

    // ── mark-to-market status ────────────────────────────────────────────────

    #[test]
    fn status_reports_a_running_return_long_before_the_horizon_matures() {
        // Only 2 bars past entry exist; a 21-day-horizon evaluate_entry would
        // return None (not matured). status_entry must still report something.
        let a = series_from(&[("2024-03-04", 100.0, 100.0), ("2024-03-05", 100.0, 110.0), ("2024-03-06", 110.0, 120.0)]);
        let mut store = PriceStore::new();
        store.insert("AAA", a);
        assert!(evaluate_entry(&entry_with(&["AAA"], "2024-03-04"), &store, None, 21).is_none());

        let s = status_entry(&entry_with(&["AAA"], "2024-03-04"), &store, None, 21);
        assert!(!s.matured);
        assert_eq!(s.picks.len(), 1);
        assert_eq!(s.picks[0].as_of, d("2024-03-06"));
        assert_eq!(s.picks[0].trading_days_held, 2);
        assert!((s.picks[0].return_so_far - 0.20).abs() < 1e-9);
        assert!((s.basket_return_so_far.unwrap() - 0.20).abs() < 1e-9);
    }

    #[test]
    fn status_marks_an_entry_matured_once_every_pick_has_enough_history() {
        let a = series_from(&[("2024-03-04", 100.0, 100.0), ("2024-03-05", 100.0, 110.0), ("2024-03-06", 110.0, 120.0)]);
        let mut store = PriceStore::new();
        store.insert("AAA", a);
        let s = status_entry(&entry_with(&["AAA"], "2024-03-04"), &store, None, 2);
        assert!(s.matured, "2 trading days held, horizon 2: fully matured");
    }

    #[test]
    fn status_of_a_cash_entry_is_always_matured_with_zero_return() {
        let bench = series_from(&[("2024-03-04", 100.0, 100.0), ("2024-03-05", 100.0, 104.0), ("2024-03-06", 104.0, 108.0)]);
        let s = status_entry(&entry_with(&[], "2024-03-04"), &PriceStore::new(), Some(&bench), 21);
        assert!(s.in_cash);
        assert!(s.matured);
        assert_eq!(s.basket_return_so_far, Some(0.0));
        assert!((s.benchmark_return_so_far.unwrap() - 0.08).abs() < 1e-9);
    }

    #[test]
    fn status_skips_a_pick_that_has_not_entered_yet() {
        // No bar after data_through at all: the pick hasn't opened a position.
        let a = series_from(&[("2024-03-04", 100.0, 100.0)]);
        let mut store = PriceStore::new();
        store.insert("AAA", a);
        let s = status_entry(&entry_with(&["AAA"], "2024-03-04"), &store, None, 21);
        assert!(s.picks.is_empty());
        assert!(s.basket_return_so_far.is_none());
        assert!(!s.matured);
    }
}
