//! Signal validation: does each signal predict subsequent returns?
//!
//! For each monthly test date we score every ticker with the signal, using only
//! data available *at that date* (prices, SEC filings by filing date, and
//! whatever news/insider history the cache holds), and correlate the scores
//! (rank IC) with the return realised afterwards.
//!
//! Honesty rules this module enforces:
//!   * A signal with no usable history is reported as **not evaluable**, never
//!     as "IC = 0, no edge". Absence of evidence is not evidence of absence.
//!   * A signal only "has edge" if its mean IC is positive *and* statistically
//!     distinguishable from zero (t-stat), not on a bare threshold.
//!   * Nothing is fitted, so the entire range is out-of-sample; `train_end` is
//!     accepted for CLI compatibility and only recorded in the report.
//!   * Known limits (survivorship, monthly overlapping windows) are listed in
//!     the report's `notes`.

use anyhow::Result;
use chrono::{Datelike, Duration, NaiveDate};
use rand::prelude::*;
use rand::rngs::StdRng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::info;

use crate::data::asof::AsOf;
use crate::data::prices::{momentum_12m1m, PriceSeries, PriceStore};
use crate::data::{cache::Cache, yahoo::YahooFinance, DataSource, FundamentalSnapshot};
use crate::metrics::ic;
use crate::universe::builder::Universe;

use super::fundamental::FundamentalSignal;
use super::insider::InsiderSignal;
use super::momentum::MomentumSignal;
use super::sentiment::SentimentSignal;
use super::{MacroSnapshot, MarketData, Signal, SignalAvailability};

/// Forward window over which a signal is judged.
const HORIZON_DAYS: i64 = 30;
/// A test date needs at least this many names with data to yield an IC.
const MIN_NAMES: usize = 5;
/// A signal needs at least this many test dates to be called evaluable.
const MIN_DATES: usize = 6;
/// Mean IC below this is not economically interesting even if "significant".
const MIN_IC: f64 = 0.02;
/// |t| needed to call an IC distinguishable from zero.
const MIN_T: f64 = 2.0;

// ── Output types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalIcResult {
    pub signal_name: String,
    pub ic_mean: f64,     // mean rank IC across test dates
    pub ic_std: f64,      // std dev of IC across test dates
    pub n_dates: usize,   // test dates that produced an IC
    /// Mean IC > 0.02 AND t-stat > 2 AND enough dates. False when not evaluable.
    pub has_edge: bool,
    #[serde(default)]
    pub t_stat: f64,
    #[serde(default)]
    pub hit_rate: f64,
    /// False when there was too little history to judge the signal at all.
    #[serde(default)]
    pub evaluable: bool,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonteCarloComparison {
    pub strategy_annualised_return: f64,
    pub random_median_return: f64,
    pub random_p5: f64,
    pub random_p95: f64,
    pub percentile_rank: f64,       // % of random portfolios we beat
    pub n_simulations: usize,
    #[serde(default)]
    pub strategy_description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    pub generated_at: String,
    pub train_period: (String, String),
    pub test_period: (String, String),
    pub signal_ic: Vec<SignalIcResult>,
    pub monte_carlo: Option<MonteCarloComparison>,
    #[serde(default)]
    pub notes: Vec<String>,
}

// ── Pure helpers (unit tested) ────────────────────────────────────────────────

/// Turn a series of per-date ICs into a verdict.
pub fn classify_ic(name: &str, ics: &[f64], unavailable_note: Option<&str>) -> SignalIcResult {
    let s = ic::summarize(ics);
    let evaluable = s.n >= MIN_DATES;
    let has_edge = evaluable && s.mean > MIN_IC && s.t_stat > MIN_T;
    let note = if evaluable {
        None
    } else {
        Some(format!(
            "not evaluable: {} test date(s) had >= {MIN_NAMES} names with this signal's data (need {MIN_DATES}){}",
            s.n,
            unavailable_note.map(|n| format!(". {n}")).unwrap_or_default(),
        ))
    };
    SignalIcResult {
        signal_name: name.to_string(),
        ic_mean: s.mean,
        ic_std: s.std,
        n_dates: s.n,
        has_edge,
        t_stat: s.t_stat,
        hit_rate: s.hit_rate,
        evaluable,
        note,
    }
}

/// Return earned by trading on the bar after `date`: enter at the next open,
/// exit at the last close on/before `date + horizon`. `None` if either end is
/// missing or the window is empty.
pub fn forward_return(series: &PriceSeries, date: NaiveDate, horizon_days: i64) -> Option<f64> {
    let entry = series.strictly_after(date)?;
    let exit = series.on_or_before(date + Duration::days(horizon_days))?;
    if exit.date <= entry.date {
        return None;
    }
    let px_in = entry.adj_open();
    (px_in.is_finite() && px_in > 0.0).then(|| exit.adj_close / px_in - 1.0)
}

/// `start`, then the first of each following month, through `end`.
pub fn monthly_dates(start: NaiveDate, end: NaiveDate) -> Vec<NaiveDate> {
    let mut dates = Vec::new();
    let mut d = start;
    while d <= end {
        dates.push(d);
        let next = if d.month() == 12 {
            NaiveDate::from_ymd_opt(d.year() + 1, 1, 1)
        } else {
            NaiveDate::from_ymd_opt(d.year(), d.month() + 1, 1)
        };
        match next {
            Some(n) => d = n,
            None => break,
        }
    }
    dates
}

/// Random portfolio returns: `n_sims` draws of `k` DISTINCT names, equal
/// weight. (The previous sampler drew with replacement, so a "10-stock"
/// portfolio could hold the same stock several times.)
pub fn random_portfolio_returns(
    returns: &[f64],
    k: usize,
    n_sims: usize,
    seed: u64,
) -> Vec<f64> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut out: Vec<f64> = (0..n_sims)
        .map(|_| {
            let picked: Vec<&f64> = returns.choose_multiple(&mut rng, k).collect();
            picked.iter().map(|r| **r).sum::<f64>() / picked.len().max(1) as f64
        })
        .collect();
    out.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    out
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn annualise(period_return: f64, years: f64) -> f64 {
    // Annualising a sub-6-month return produces meaningless extremes.
    if years >= 0.5 && period_return > -1.0 {
        (1.0 + period_return).powf(1.0 / years) - 1.0
    } else {
        period_return
    }
}

// ── Validator ─────────────────────────────────────────────────────────────────

pub struct WalkForwardValidator {
    cache: Cache,
    yahoo: YahooFinance,
}

/// One ticker's inputs at one test date.
struct Row {
    data: MarketData,
    availability: SignalAvailability,
    fwd: Option<f64>,
}

impl WalkForwardValidator {
    pub fn new(cache: Cache) -> Self {
        let yahoo = YahooFinance::new(cache.clone());
        Self { cache, yahoo }
    }

    pub async fn validate(
        &self,
        universe: &Universe,
        train_end: NaiveDate,
        test_start: NaiveDate,
        test_end: NaiveDate,
        run_monte_carlo: bool,
    ) -> Result<ValidationReport> {
        info!("Signal validation: {} → {} (nothing fitted; all out-of-sample)", test_start, test_end);

        let mut slots: Vec<_> = universe.all_slots().collect();
        slots.sort_by(|a, b| a.ticker.cmp(&b.ticker));
        let test_dates = monthly_dates(test_start, test_end);

        // Prices once, for the whole range plus lookback and forward window.
        let mut prices = PriceStore::new();
        for slot in &slots {
            let bars = self
                .yahoo
                .price_history(
                    &slot.ticker,
                    test_start - Duration::days(400),
                    test_end + Duration::days(HORIZON_DAYS + 10),
                )
                .await
                .unwrap_or_default();
            prices.insert(slot.ticker.clone(), PriceSeries::new(bars));
        }

        let signals: [(&str, &dyn Signal, fn(&SignalAvailability) -> bool, &str); 4] = [
            ("Momentum", &MomentumSignal, |a| a.momentum, ""),
            ("Fundamental", &FundamentalSignal, |a| a.fundamental,
             "Point-in-time fundamentals exist for US issuers only (SEC filings)."),
            ("Insider", &InsiderSignal, |a| a.insider,
             "Insider history is only what the cache has collected; it builds up going forward."),
            ("Sentiment", &SentimentSignal, |a| a.sentiment,
             "GDELT/Reddit cannot be reconstructed for past dates; history builds up going forward."),
        ];
        let mut ic_series: Vec<Vec<f64>> = vec![Vec::new(); signals.len()];

        for &date in &test_dates {
            let rows = self.build_rows(&slots, &prices, date).await;
            for (i, (_, signal, available, _)) in signals.iter().enumerate() {
                let (mut xs, mut ys) = (Vec::new(), Vec::new());
                for row in &rows {
                    if !available(&row.availability) {
                        continue; // no data for THIS signal: don't score it as neutral
                    }
                    if let Some(fwd) = row.fwd {
                        xs.push(signal.compute(&row.data.ticker, &row.data));
                        ys.push(fwd);
                    }
                }
                if xs.len() >= MIN_NAMES {
                    if let Some(v) = ic::spearman(&xs, &ys) {
                        ic_series[i].push(v);
                    }
                }
            }
        }

        let signal_ic: Vec<SignalIcResult> = signals
            .iter()
            .zip(&ic_series)
            .map(|((name, _, _, note), series)| {
                let r = classify_ic(name, series, (!note.is_empty()).then_some(*note));
                info!(signal = %name, ic = %format!("{:.4}", r.ic_mean), t = %format!("{:.2}", r.t_stat),
                      dates = r.n_dates, edge = r.has_edge, evaluable = r.evaluable, "IC computed");
                r
            })
            .collect();

        let monte_carlo = if run_monte_carlo {
            self.run_monte_carlo(&prices, &slots, test_start, test_end)
        } else {
            None
        };

        Ok(ValidationReport {
            generated_at: chrono::Local::now().to_rfc3339(),
            train_period: ("(none - nothing is fitted)".to_string(), train_end.to_string()),
            test_period: (test_start.to_string(), test_end.to_string()),
            signal_ic,
            monte_carlo,
            notes: vec![
                "Universe is today's ticker list: survivorship bias (delisted names are absent) flatters every result.".into(),
                format!("Forward returns use a {HORIZON_DAYS}-day window sampled monthly; consecutive windows overlap slightly, so t-stats are indicative, not exact."),
                "Rank (Spearman) IC. Entry at the next open after the test date, exit at the last close within the window.".into(),
            ],
        })
    }

    /// Point-in-time inputs for every ticker at `date`.
    async fn build_rows(
        &self,
        slots: &[&crate::universe::builder::CompanySlot],
        prices: &PriceStore,
        date: NaiveDate,
    ) -> Vec<Row> {
        let view = AsOf::new(&self.cache, date);
        let macro_snapshot = MacroSnapshot::neutral(date);

        // Per-ticker bars/fundamentals first, so peers can be formed per industry.
        struct Raw {
            ticker: String,
            industry: String,
            bars: Vec<crate::data::PriceBar>,
            fundamentals: Option<FundamentalSnapshot>,
        }
        let mut raws: Vec<Raw> = Vec::new();
        for slot in slots {
            let bars = prices
                .get(&slot.ticker)
                .map(|s| s.window(date - Duration::days(400), date).to_vec())
                .unwrap_or_default();
            let fundamentals = self.yahoo.fundamentals(&slot.ticker, date).await.ok();
            raws.push(Raw {
                ticker: slot.ticker.clone(),
                industry: slot.industry_name.clone(),
                bars,
                fundamentals,
            });
        }

        let mut peer_returns: HashMap<&str, HashMap<String, f64>> = HashMap::new();
        let mut peer_funds: HashMap<&str, HashMap<String, FundamentalSnapshot>> = HashMap::new();
        for r in &raws {
            if let Some(m) = momentum_12m1m(&r.bars) {
                peer_returns.entry(r.industry.as_str()).or_default().insert(r.ticker.clone(), m);
            }
            if let Some(f) = &r.fundamentals {
                peer_funds.entry(r.industry.as_str()).or_default().insert(r.ticker.clone(), f.clone());
            }
        }

        raws.iter()
            .map(|r| {
                let data = MarketData {
                    ticker: r.ticker.clone(),
                    as_of: date,
                    industry_name: r.industry.clone(),
                    price_bars: r.bars.clone(),
                    fundamentals: r.fundamentals.clone(),
                    peer_returns_12m1m: peer_returns.get(r.industry.as_str()).cloned().unwrap_or_default(),
                    peer_fundamentals: peer_funds.get(r.industry.as_str()).cloned().unwrap_or_default(),
                    insider_trades: view.insider_trades(&r.ticker, 90).unwrap_or_default(),
                    news_items: view.news_items(&r.ticker, 30).unwrap_or_default(),
                    reddit_snapshots: view.reddit_snapshots(&r.ticker, 7).unwrap_or_default(),
                    macro_snapshot: macro_snapshot.clone(),
                };
                let availability = SignalAvailability::assess(&data, false);
                let fwd = prices.get(&r.ticker).and_then(|s| forward_return(s, date, HORIZON_DAYS));
                Row { data, availability, fwd }
            })
            .collect()
    }

    // ── Monte Carlo ───────────────────────────────────────────────────────────

    /// Compare a simple, fully point-in-time strategy (top-10 by 12-1 momentum
    /// at `test_start`, held to `test_end`) with random 10-name portfolios.
    /// This is a momentum proxy, NOT the full composite — labelled as such.
    fn run_monte_carlo(
        &self,
        prices: &PriceStore,
        slots: &[&crate::universe::builder::CompanySlot],
        test_start: NaiveDate,
        test_end: NaiveDate,
    ) -> Option<MonteCarloComparison> {
        const N_SIMS: usize = 1_000;
        const N_STOCKS: usize = 10;

        // Buy-and-hold return of every name over the whole window.
        let period_return = |t: &str| -> Option<f64> {
            let s = prices.get(t)?;
            let entry = s.strictly_after(test_start)?;
            let exit = s.on_or_before(test_end)?;
            let px = entry.adj_open();
            (exit.date > entry.date && px > 0.0).then(|| exit.adj_close / px - 1.0)
        };

        let mut universe_returns: Vec<(String, f64)> = slots
            .iter()
            .filter_map(|s| period_return(&s.ticker).map(|r| (s.ticker.clone(), r)))
            .collect();
        universe_returns.sort_by(|a, b| a.0.cmp(&b.0));
        if universe_returns.len() <= N_STOCKS {
            return None;
        }

        // Strategy picks use only bars up to test_start.
        let mut momentum: Vec<(String, f64)> = slots
            .iter()
            .filter_map(|s| {
                let bars = prices.get(&s.ticker)?.window(test_start - Duration::days(400), test_start);
                momentum_12m1m(bars).map(|m| (s.ticker.clone(), m))
            })
            .collect();
        momentum.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.0.cmp(&b.0)));
        let picked: Vec<f64> = momentum
            .iter()
            .take(N_STOCKS)
            .filter_map(|(t, _)| period_return(t))
            .collect();
        if picked.len() < N_STOCKS {
            return None;
        }
        let strategy_return = picked.iter().sum::<f64>() / picked.len() as f64;

        let all: Vec<f64> = universe_returns.iter().map(|(_, r)| *r).collect();
        let sims = random_portfolio_returns(&all, N_STOCKS, N_SIMS, 12_345);

        let years = (test_end - test_start).num_days() as f64 / 365.25;
        let ann = |r: f64| annualise(r, years);
        let beaten = sims.iter().filter(|&&r| r < strategy_return).count();

        Some(MonteCarloComparison {
            strategy_annualised_return: ann(strategy_return),
            random_median_return: ann(percentile(&sims, 50.0)),
            random_p5: ann(percentile(&sims, 5.0)),
            random_p95: ann(percentile(&sims, 95.0)),
            percentile_rank: beaten as f64 / sims.len() as f64 * 100.0,
            n_simulations: N_SIMS,
            strategy_description: format!(
                "top-{N_STOCKS} by 12-1 month momentum at test start, buy & hold (a proxy, not the composite); returns {}",
                if years >= 0.5 { "annualised" } else { "un-annualised (window < 6 months)" }
            ),
        })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::prices::test_bar;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn a_signal_needs_history_before_it_can_be_judged() {
        // Regression: empty inputs used to yield IC = 0 and a confident "no edge".
        let r = classify_ic("Insider", &[], Some("history builds up going forward"));
        assert!(!r.evaluable);
        assert!(!r.has_edge);
        assert!(r.note.as_deref().unwrap().contains("not evaluable"));
        assert!(r.note.as_deref().unwrap().contains("going forward"));

        let few = classify_ic("X", &[0.2, 0.3, 0.25], None);
        assert!(!few.evaluable && !few.has_edge, "3 dates is not enough, however good");
    }

    #[test]
    fn edge_requires_a_significant_positive_ic_not_just_a_threshold() {
        // Consistently positive → edge.
        let good = classify_ic("G", &[0.06, 0.05, 0.07, 0.04, 0.06, 0.05, 0.08], None);
        assert!(good.evaluable && good.has_edge, "t={}", good.t_stat);

        // Same mean but wildly noisy → not distinguishable from zero.
        let noisy = classify_ic("N", &[0.5, -0.4, 0.45, -0.35, 0.4, -0.3, 0.3], None);
        assert!(noisy.evaluable);
        assert!(noisy.ic_mean > 0.02 && !noisy.has_edge, "noisy IC must not pass on its mean alone (t={})", noisy.t_stat);

        // Significantly NEGATIVE is not an edge either.
        let bad = classify_ic("B", &[-0.06, -0.05, -0.07, -0.04, -0.06, -0.05, -0.08], None);
        assert!(!bad.has_edge);
    }

    #[test]
    fn forward_return_enters_at_the_next_open_not_the_test_date_close() {
        let mut bars = vec![
            test_bar("2024-01-02", 100.0), // test date close
            test_bar("2024-01-03", 100.0),
            test_bar("2024-01-31", 130.0),
        ];
        bars[1].open = 110.0; // next-bar open
        bars[1].close = 105.0;
        bars[1].adj_close = 105.0; // keep the adjustment factor at 1
        let s = PriceSeries::new(bars);
        // Enter 110 (next open), exit 130 (last close within 30d of 1/2 → 2/1).
        let r = forward_return(&s, d("2024-01-02"), 30).unwrap();
        assert!((r - (130.0 / 110.0 - 1.0)).abs() < 1e-9, "{r}");
    }

    #[test]
    fn forward_return_is_none_without_a_next_bar_or_a_window() {
        let s = PriceSeries::new(vec![test_bar("2024-01-02", 100.0)]);
        assert!(forward_return(&s, d("2024-01-02"), 30).is_none(), "no bar after the date");
        let s2 = PriceSeries::new(vec![test_bar("2024-01-02", 100.0), test_bar("2024-06-01", 100.0)]);
        // Entry 6/1 but the exit window ends 2/1 → exit precedes entry.
        assert!(forward_return(&s2, d("2024-01-02"), 30).is_none());
    }

    #[test]
    fn monthly_dates_start_then_first_of_each_month() {
        assert_eq!(
            monthly_dates(d("2023-11-15"), d("2024-02-10")),
            vec![d("2023-11-15"), d("2023-12-01"), d("2024-01-01"), d("2024-02-01")]
        );
        assert_eq!(monthly_dates(d("2024-01-01"), d("2024-01-01")), vec![d("2024-01-01")]);
    }

    #[test]
    fn random_portfolios_hold_distinct_names() {
        // Universe of 10 names, portfolio of 10: with replacement the sample
        // mean would vary; without replacement it is exactly the universe mean.
        let returns: Vec<f64> = (0..10).map(|i| i as f64 / 10.0).collect();
        let mean = returns.iter().sum::<f64>() / 10.0;
        let sims = random_portfolio_returns(&returns, 10, 200, 3);
        assert!(sims.iter().all(|s| (s - mean).abs() < 1e-12), "every draw must be the whole universe");
    }

    #[test]
    fn random_portfolios_vary_and_are_reproducible() {
        let returns: Vec<f64> = (0..40).map(|i| i as f64 / 40.0).collect();
        let a = random_portfolio_returns(&returns, 10, 500, 9);
        let b = random_portfolio_returns(&returns, 10, 500, 9);
        assert_eq!(a, b);
        assert!(percentile(&a, 95.0) > percentile(&a, 5.0));
    }

    #[test]
    fn annualisation_only_for_windows_of_six_months_or_more() {
        assert!((annualise(0.21, 2.0) - (1.21f64.sqrt() - 1.0)).abs() < 1e-12);
        assert_eq!(annualise(0.10, 0.25), 0.10, "a 3-month return is not annualised");
        assert_eq!(annualise(-1.5, 2.0), -1.5, "guard against invalid returns");
    }
}
