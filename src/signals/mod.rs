pub mod momentum;
pub mod fundamental;
pub mod insider;
pub mod sentiment;
pub mod macro_filter;
pub mod engine;
pub mod backtest;

pub use engine::PickingEngine;
pub use backtest::WalkForwardValidator;

use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use chrono::NaiveDate;
use anyhow::Result;
use async_trait::async_trait;
use crate::universe::builder::Universe;

use crate::data::{
    FundamentalSnapshot, InsiderTrade, MacroSnapshot, NewsItem, PriceBar, RedditSnapshot,
};

// ── Signal trait ──────────────────────────────────────────────────────────────

/// Each signal module implements this. `compute` is intentionally synchronous —
/// all async fetching is done by the engine before calling signals.
/// Returns a score in [-1.0, +1.0]:  +1 = strongest buy, -1 = strongest sell.
pub trait Signal: Send + Sync {
    fn name(&self) -> &str;
    fn compute(&self, ticker: &str, data: &MarketData) -> f64;
}

// ── Ranker ────────────────────────────────────────────────────────────────────

/// Anything that can rank a universe as of a date. The backtester depends on
/// this trait rather than on `PickingEngine`, so it can be tested with a
/// deterministic ranker and no network.
#[async_trait]
pub trait Ranker: Send + Sync {
    async fn rank(&self, universe: &Universe, as_of: NaiveDate) -> Result<Vec<SignalScore>>;
}

// ── Signal availability ───────────────────────────────────────────────────────

/// Which signals had real input data for a ticker on a given date.
///
/// A signal with no data returns 0.0 ("neutral"), which is indistinguishable
/// from a genuinely neutral reading. Tracking availability makes that visible
/// and lets the composite ignore signals that carry no information.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalAvailability {
    pub momentum:    bool,
    pub fundamental: bool,
    pub insider:     bool,
    pub sentiment:   bool,
    pub pairs:       bool,
}

impl SignalAvailability {
    pub fn all() -> Self {
        Self { momentum: true, fundamental: true, insider: true, sentiment: true, pairs: true }
    }

    pub fn count(&self) -> usize {
        [self.momentum, self.fundamental, self.insider, self.sentiment, self.pairs]
            .iter()
            .filter(|b| **b)
            .count()
    }

    pub fn missing(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.momentum    { out.push("momentum"); }
        if !self.fundamental { out.push("fundamental"); }
        if !self.insider     { out.push("insider"); }
        if !self.sentiment   { out.push("sentiment"); }
        if !self.pairs       { out.push("pairs"); }
        out
    }

    /// Assess which signals have usable inputs.
    ///
    /// `insider` is "any filings in the window": an empty window can mean
    /// "nobody traded" or "no data", and we cannot tell them apart, so both
    /// are treated as *no information* and excluded from the composite.
    pub fn assess(data: &MarketData, pairs_available: bool) -> Self {
        let fundamental = data.fundamentals.as_ref().map_or(false, |f| {
            [
                f.revenue_ttm.is_some(),
                f.net_margin_pct.is_some(),
                f.debt_to_equity.is_some(),
                f.price_to_book.is_some(),
                f.operating_cashflow.is_some(),
                f.return_on_assets.is_some(),
            ]
            .iter()
            .filter(|b| **b)
            .count()
                >= 3
        });
        Self {
            momentum:    data.price_bars.len() >= 50,
            fundamental,
            insider:     !data.insider_trades.is_empty(),
            sentiment:   !data.news_items.is_empty() || !data.reddit_snapshots.is_empty(),
            pairs:       pairs_available,
        }
    }
}

// ── Market data bundle ────────────────────────────────────────────────────────

/// All data needed to compute every signal for one ticker.
/// Assembled by the PickingEngine before calling each Signal.
pub struct MarketData {
    pub ticker: String,
    pub as_of: NaiveDate,
    pub industry_name: String,

    // Price history — ~13 months for 12-1m momentum + MA computation
    pub price_bars: Vec<PriceBar>,

    // Point-in-time fundamentals
    pub fundamentals: Option<FundamentalSnapshot>,

    // Industry peers for relative comparison
    // peer ticker → 12-1m price return
    pub peer_returns_12m1m: HashMap<String, f64>,
    // peer ticker → fundamentals
    pub peer_fundamentals: HashMap<String, FundamentalSnapshot>,

    // SEC EDGAR Form 4 data (last 90 days)
    pub insider_trades: Vec<InsiderTrade>,

    // GDELT news (last 30 days)
    pub news_items: Vec<NewsItem>,

    // Reddit snapshots (last 7 days, across subreddits)
    pub reddit_snapshots: Vec<RedditSnapshot>,

    // FRED macro regime snapshot
    pub macro_snapshot: MacroSnapshot,
}

// ── Signal score per ticker ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalScore {
    pub rank: usize,
    pub ticker: String,
    pub industry: String,
    pub composite: f64,      // 0–100

    // Raw signal scores in [-1, +1]
    pub momentum_raw: f64,
    pub fundamental_raw: f64,
    pub insider_raw: f64,
    pub sentiment_raw: f64,
    pub pairs_raw: f64,

    // Scaled contributions (0–100 per signal, weighted)
    pub momentum_contrib: f64,
    pub fundamental_contrib: f64,
    pub insider_contrib: f64,
    pub sentiment_contrib: f64,
    pub pairs_contrib: f64,

    pub macro_on: bool,

    /// Which signals had real data. Old serialized scores lack this field.
    #[serde(default)]
    pub availability: SignalAvailability,
}

impl SignalScore {
    /// Number of the five signals that carried real data.
    pub fn signals_available(&self) -> usize {
        self.availability.count()
    }
}

// ── Signal weights ────────────────────────────────────────────────────────────

/// Configurable via environment variables (MOMENTUM_WEIGHT etc.).
/// Must sum to 1.0; normalised if they don't.
/// Pairs weight defaults to 0.15; existing weights scale down proportionally.
#[derive(Debug, Clone)]
pub struct SignalWeights {
    pub momentum: f64,
    pub fundamental: f64,
    pub insider: f64,
    pub sentiment: f64,
    pub pairs: f64,
}

impl Default for SignalWeights {
    fn default() -> Self {
        fn env_f64(key: &str, default: f64) -> f64 {
            std::env::var(key)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(default)
        }
        let mut w = Self {
            momentum:    env_f64("MOMENTUM_WEIGHT",    0.30),
            fundamental: env_f64("FUNDAMENTAL_WEIGHT", 0.25),
            insider:     env_f64("INSIDER_WEIGHT",     0.25),
            sentiment:   env_f64("SENTIMENT_WEIGHT",   0.20),
            pairs:       env_f64("PAIRS_WEIGHT",       0.15),
        };
        // Normalise so weights always sum to 1.0
        let total = w.momentum + w.fundamental + w.insider + w.sentiment + w.pairs;
        if total > 0.0 {
            w.momentum    /= total;
            w.fundamental /= total;
            w.insider     /= total;
            w.sentiment   /= total;
            w.pairs       /= total;
        }
        w
    }
}

// ── Composite scoring ─────────────────────────────────────────────────────────

/// Compute a `SignalScore` from raw signal outputs.
///
/// Weights are renormalised over the signals that actually had data, so a
/// missing signal no longer drags the composite toward a fake neutral 50.
/// The macro regime is **not** baked into the score: it is reported in
/// `macro_on` and enforced at selection time by [`select_picks`]. Ranks stay
/// meaningful in risk-off, and "hold cash" is an explicit decision rather than
/// a side effect of every score collapsing to 50.
pub fn composite_score(
    ticker: &str,
    industry: &str,
    momentum_raw: f64,
    fundamental_raw: f64,
    insider_raw: f64,
    sentiment_raw: f64,
    pairs_raw: f64,
    macro_on: bool,
    weights: &SignalWeights,
    availability: &SignalAvailability,
) -> SignalScore {
    let w = [
        if availability.momentum    { weights.momentum }    else { 0.0 },
        if availability.fundamental { weights.fundamental } else { 0.0 },
        if availability.insider     { weights.insider }     else { 0.0 },
        if availability.sentiment   { weights.sentiment }   else { 0.0 },
        if availability.pairs       { weights.pairs }       else { 0.0 },
    ];
    let raws = [momentum_raw, fundamental_raw, insider_raw, sentiment_raw, pairs_raw];
    let total_w: f64 = w.iter().sum();

    let raw_weighted = if total_w > 1e-12 {
        w.iter().zip(raws.iter()).map(|(wi, ri)| wi * ri).sum::<f64>() / total_w
    } else {
        0.0
    };

    // Scale [-1, +1] → [0, 100]
    let scale = |v: f64| -> f64 { ((v + 1.0) / 2.0 * 100.0).clamp(0.0, 100.0) };

    SignalScore {
        rank: 0, // set by caller after sorting
        ticker: ticker.to_string(),
        industry: industry.to_string(),
        composite: scale(raw_weighted),
        momentum_raw,
        fundamental_raw,
        insider_raw,
        sentiment_raw,
        pairs_raw,
        momentum_contrib:    scale(momentum_raw),
        fundamental_contrib: scale(fundamental_raw),
        insider_contrib:     scale(insider_raw),
        sentiment_contrib:   scale(sentiment_raw),
        pairs_contrib:       scale(pairs_raw),
        macro_on,
        availability: *availability,
    }
}

fn by_composite_desc(a: &SignalScore, b: &SignalScore) -> std::cmp::Ordering {
    b.composite
        .partial_cmp(&a.composite)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| a.ticker.cmp(&b.ticker))
}

/// Sort descending by composite; ties broken by ticker so results are
/// reproducible run to run (HashMap iteration order is not), then assign ranks.
pub fn sort_and_rank(scores: &mut [SignalScore]) {
    scores.sort_by(by_composite_desc);
    for (i, s) in scores.iter_mut().enumerate() {
        s.rank = i + 1;
    }
}

/// The single source of truth for "which names do we hold".
///
/// * Risk-off (any score has `macro_on == false`) with `macro_gate` enabled →
///   no picks: hold cash.
/// * Otherwise: drop scores below `min_score`, rank, take `top_n`.
pub fn select_picks(
    scores: &[SignalScore],
    top_n: usize,
    min_score: Option<f64>,
    macro_gate: bool,
) -> Vec<SignalScore> {
    if macro_gate && scores.iter().any(|s| !s.macro_on) {
        return Vec::new();
    }
    let mut picks: Vec<SignalScore> = scores
        .iter()
        .filter(|s| min_score.map_or(true, |m| s.composite >= m))
        .cloned()
        .collect();
    picks.sort_by(by_composite_desc);
    picks.truncate(top_n);
    picks
}

// ── Statistics helpers shared across signal modules ───────────────────────────

pub fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

pub fn std_dev(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let m = mean(values);
    let v = values.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (values.len() - 1) as f64;
    v.sqrt()
}

/// Clamp a value to [-1, +1] — used by all signal `compute()` implementations.
pub fn clamp_signal(v: f64) -> f64 {
    v.clamp(-1.0, 1.0)
}

/// z-score then clamp: maps an absolute metric into a normalised signal score.
/// `higher_is_better`: if false, negate before clamping.
pub fn zscore_signal(value: f64, universe: &[f64], higher_is_better: bool) -> f64 {
    if universe.is_empty() {
        return 0.0;
    }
    let m = mean(universe);
    let s = std_dev(universe);
    if s < 1e-9 {
        return 0.0;
    }
    let z = (value - m) / s;
    let z = if higher_is_better { z } else { -z };
    // Divide by 3 so ±3σ maps to ±1.0
    clamp_signal(z / 3.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weights() -> SignalWeights {
        SignalWeights { momentum: 0.4, fundamental: 0.3, insider: 0.1, sentiment: 0.1, pairs: 0.1 }
    }

    fn score(t: &str, raw: f64, macro_on: bool) -> SignalScore {
        let a = SignalAvailability::all();
        composite_score(t, "Ind", raw, raw, raw, raw, raw, macro_on, &weights(), &a)
    }

    #[test]
    fn composite_maps_signal_range_to_0_100() {
        assert!((score("A", 1.0, true).composite - 100.0).abs() < 1e-9);
        assert!((score("A", -1.0, true).composite - 0.0).abs() < 1e-9);
        assert!((score("A", 0.0, true).composite - 50.0).abs() < 1e-9);
    }

    #[test]
    fn missing_signals_are_excluded_not_treated_as_neutral() {
        // Only momentum has data and it is strongly positive.
        let only_mom = SignalAvailability { momentum: true, ..Default::default() };
        let s = composite_score("A", "I", 1.0, 0.0, 0.0, 0.0, 0.0, true, &weights(), &only_mom);
        assert!((s.composite - 100.0).abs() < 1e-9, "got {}", s.composite);

        // Same raw values but everything "available": the zeros drag it down.
        let full = composite_score("A", "I", 1.0, 0.0, 0.0, 0.0, 0.0, true, &weights(), &SignalAvailability::all());
        assert!(full.composite < 80.0);
    }

    #[test]
    fn no_available_signals_is_neutral_50() {
        let s = composite_score("A", "I", 1.0, 1.0, 1.0, 1.0, 1.0, true, &weights(), &SignalAvailability::default());
        assert!((s.composite - 50.0).abs() < 1e-9);
    }

    #[test]
    fn macro_off_keeps_real_scores_and_ranks() {
        let a = score("A", 0.8, false);
        let b = score("B", -0.8, false);
        assert!(a.composite > b.composite, "ranking must survive risk-off");
        assert!(!a.macro_on);
    }

    #[test]
    fn risk_off_selects_nothing_when_gate_enabled() {
        let scores = vec![score("A", 0.8, false), score("B", 0.7, false)];
        assert!(select_picks(&scores, 5, None, true).is_empty());
    }

    #[test]
    fn risk_off_ignored_when_gate_disabled() {
        let scores = vec![score("A", 0.8, false), score("B", 0.7, false)];
        assert_eq!(select_picks(&scores, 5, None, false).len(), 2);
    }

    #[test]
    fn select_respects_top_n_and_min_score() {
        let scores = vec![score("A", 0.9, true), score("B", 0.5, true), score("C", -0.5, true)];
        let top = select_picks(&scores, 2, None, true);
        assert_eq!(top.iter().map(|s| s.ticker.as_str()).collect::<Vec<_>>(), ["A", "B"]);
        let filtered = select_picks(&scores, 10, Some(70.0), true);
        assert_eq!(filtered.len(), 2); // 95 and 75 clear 70, 25 does not
    }

    #[test]
    fn ties_break_by_ticker_for_reproducibility() {
        let mut scores = vec![score("ZED", 0.3, true), score("ABC", 0.3, true), score("MID", 0.3, true)];
        sort_and_rank(&mut scores);
        assert_eq!(scores.iter().map(|s| s.ticker.as_str()).collect::<Vec<_>>(), ["ABC", "MID", "ZED"]);
        assert_eq!(scores[0].rank, 1);
        let picked = select_picks(&scores, 2, None, true);
        assert_eq!(picked[0].ticker, "ABC");
    }

    #[test]
    fn zscore_signal_direction_and_clamp() {
        let u = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert!(zscore_signal(5.0, &u, true) > 0.0);
        assert!(zscore_signal(5.0, &u, false) < 0.0);
        assert!(zscore_signal(1_000.0, &u, true) <= 1.0);
        assert_eq!(zscore_signal(3.0, &[2.0, 2.0], true), 0.0); // no variance
    }
}
