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
) -> SignalScore {
    let (raw_weighted, m_c, f_c, i_c, s_c, p_c) = if macro_on {
        let m = weights.momentum    * momentum_raw;
        let f = weights.fundamental * fundamental_raw;
        let i = weights.insider     * insider_raw;
        let s = weights.sentiment   * sentiment_raw;
        let p = weights.pairs       * pairs_raw;
        (m + f + i + s + p, m, f, i, s, p)
    } else {
        (0.0, 0.0, 0.0, 0.0, 0.0, 0.0)
    };

    // Scale weighted sum from [-1,+1] to [0,100]
    let composite = ((raw_weighted + 1.0) / 2.0 * 100.0).clamp(0.0, 100.0);

    // Contributions are each signal's weighted value scaled to [0,max_contribution]
    let scale = |v: f64| -> f64 { ((v + 1.0) / 2.0 * 100.0).clamp(0.0, 100.0) };

    SignalScore {
        rank: 0, // set by caller after sorting
        ticker: ticker.to_string(),
        industry: industry.to_string(),
        composite,
        momentum_raw,
        fundamental_raw,
        insider_raw,
        sentiment_raw,
        pairs_raw,
        momentum_contrib:    scale(m_c / weights.momentum.max(1e-9)),
        fundamental_contrib: scale(f_c / weights.fundamental.max(1e-9)),
        insider_contrib:     scale(i_c / weights.insider.max(1e-9)),
        sentiment_contrib:   scale(s_c / weights.sentiment.max(1e-9)),
        pairs_contrib:       scale(p_c / weights.pairs.max(1e-9)),
        macro_on,
    }
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
