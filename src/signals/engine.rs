use anyhow::Result;
use chrono::{Duration, NaiveDate};
use std::collections::HashMap;
use tracing::{info, warn};

use crate::correlations::{CorrelationEngine, PairsSignal};
use crate::data::{
    cache::Cache, edgar::EdgarFetcher, fred::FredFetcher, gdelt::GdeltFetcher,
    reddit::RedditFetcher, yahoo::YahooFinance, DataSource, FundamentalSnapshot, MacroSnapshot,
};
use crate::llm::StrategySpec;
use crate::universe::builder::Universe;

use super::{
    composite_score,
    fundamental::FundamentalSignal,
    insider::InsiderSignal,
    macro_filter::MacroFilter,
    momentum::MomentumSignal,
    sentiment::SentimentSignal,
    MarketData, Signal, SignalScore, SignalWeights,
};

// ── PickingEngine ─────────────────────────────────────────────────────────────

pub struct PickingEngine {
    cache:         Cache,
    yahoo:         YahooFinance,
    edgar:         EdgarFetcher,
    gdelt:         GdeltFetcher,
    reddit:        RedditFetcher,
    fred:          FredFetcher,
    pub weights:   SignalWeights,
    strategy_spec: Option<StrategySpec>,
}

impl PickingEngine {
    pub fn new(cache: Cache) -> Self {
        let yahoo  = YahooFinance::new(cache.clone());
        let edgar  = EdgarFetcher::new(cache.clone());
        let gdelt  = GdeltFetcher::new(cache.clone());
        let reddit = RedditFetcher::new(cache.clone());
        let fred   = FredFetcher::new(cache.clone());
        Self {
            cache,
            yahoo,
            edgar,
            gdelt,
            reddit,
            fred,
            weights:       SignalWeights::default(),
            strategy_spec: None,
        }
    }

    /// Override signal weights and store the full spec for rank_universe filtering.
    /// Non-null weight fields replace defaults; null fields keep the engine's current weights.
    pub fn apply_strategy_spec(&mut self, spec: &StrategySpec) {
        let sw = &spec.signal_weights;
        if let Some(v) = sw.momentum    { self.weights.momentum    = v; }
        if let Some(v) = sw.fundamental { self.weights.fundamental = v; }
        if let Some(v) = sw.insider     { self.weights.insider     = v; }
        if let Some(v) = sw.sentiment   { self.weights.sentiment   = v; }
        if let Some(v) = sw.pairs       { self.weights.pairs       = v; }

        // Re-normalise so weights always sum to 1.0
        let total = self.weights.momentum + self.weights.fundamental
            + self.weights.insider + self.weights.sentiment + self.weights.pairs;
        if total > 0.0 {
            self.weights.momentum    /= total;
            self.weights.fundamental /= total;
            self.weights.insider     /= total;
            self.weights.sentiment   /= total;
            self.weights.pairs       /= total;
        }

        self.strategy_spec = Some(spec.clone());
    }

    /// Rank every ticker in the universe and return a sorted `Vec<SignalScore>`.
    /// `as_of` is the scoring date (use today for live picks).
    pub async fn rank_universe(
        &self,
        universe: &Universe,
        as_of: NaiveDate,
    ) -> Result<Vec<SignalScore>> {
        // ── 1. Fetch macro regime once (shared across all tickers) ────────────
        let macro_snapshot = self.fred.macro_snapshot(as_of).await;
        let macro_on = MacroFilter::compute_macro_on(&macro_snapshot);
        info!(
            as_of = %as_of,
            macro_on = %macro_on,
            vix = ?macro_snapshot.vix,
            "Macro regime computed"
        );

        // ── 2. Pre-compute correlation matrix for pairs signal ────────────────
        let industry_tickers = universe.industry_ticker_map();
        let corr_engine = CorrelationEngine::new(self.cache.clone());
        let pairs_signal: Option<PairsSignal> = match corr_engine.load_or_compute(&industry_tickers, as_of) {
            Ok(data) => Some(PairsSignal::new(data)),
            Err(e) => {
                warn!("Correlation engine skipped (pairs signal = 0): {e:#}");
                None
            }
        };

        // ── 3. Score each ticker ──────────────────────────────────────────────
        // Build optional allow-list from universe_override
        let ticker_filter: Option<std::collections::HashSet<String>> =
            self.strategy_spec.as_ref().and_then(|spec| {
                spec.universe_override.as_ref().map(|tickers| {
                    tickers.iter().map(|t| t.to_uppercase()).collect()
                })
            });

        // Collect sector allow/deny lists from spec
        let sector_allow: Option<Vec<String>> = self.strategy_spec.as_ref()
            .and_then(|s| s.filters.sectors.clone())
            .map(|v| v.iter().map(|s| s.to_lowercase()).collect());
        let sector_deny: Option<Vec<String>> = self.strategy_spec.as_ref()
            .and_then(|s| s.filters.exclude_sectors.clone())
            .map(|v| v.iter().map(|s| s.to_lowercase()).collect());

        let mut scores: Vec<SignalScore> = Vec::new();

        for (industry_code, slots) in &universe.by_industry {
            if slots.is_empty() {
                continue;
            }

            let industry_name = slots[0].industry_name.as_str();

            // Pre-compute peer returns and fundamentals for relative comparisons
            let (peer_returns, peer_fundamentals) =
                self.gather_peer_data(slots, as_of).await;

            for slot in slots {
                let ticker = &slot.ticker;

                // Apply universe_override filter
                if let Some(ref allowed) = ticker_filter {
                    if !allowed.contains(&ticker.to_uppercase()) {
                        continue;
                    }
                }

                // Apply sector allow/deny
                let slot_sector = slot.sector_name.to_lowercase();
                if let Some(ref allow) = sector_allow {
                    if !slot_sector.is_empty() && !allow.contains(&slot_sector) {
                        continue;
                    }
                }
                if let Some(ref deny) = sector_deny {
                    if !slot_sector.is_empty() && deny.contains(&slot_sector) {
                        continue;
                    }
                }

                match self
                    .build_market_data(
                        ticker,
                        industry_name,
                        as_of,
                        &peer_returns,
                        &peer_fundamentals,
                        macro_snapshot.clone(),
                    )
                    .await
                {
                    Ok(data) => {
                        let score = self.score(&data, pairs_signal.as_ref());
                        scores.push(score);
                    }
                    Err(e) => {
                        warn!(ticker = %ticker, "MarketData gather failed: {:#}", e);
                        // Push a zero-score entry so the ticker still appears
                        scores.push(SignalScore {
                            rank: 0,
                            ticker: ticker.clone(),
                            industry: industry_name.to_string(),
                            composite: 50.0,
                            momentum_raw: 0.0,
                            fundamental_raw: 0.0,
                            insider_raw: 0.0,
                            sentiment_raw: 0.0,
                            pairs_raw: 0.0,
                            momentum_contrib: 50.0,
                            fundamental_contrib: 50.0,
                            insider_contrib: 50.0,
                            sentiment_contrib: 50.0,
                            pairs_contrib: 50.0,
                            macro_on,
                        });
                    }
                }
            }
        }

        // ── 4. Sort descending, assign ranks ──────────────────────────────────
        scores.sort_by(|a, b| b.composite.partial_cmp(&a.composite).unwrap_or(std::cmp::Ordering::Equal));
        for (i, s) in scores.iter_mut().enumerate() {
            s.rank = i + 1;
        }

        info!(
            "PickingEngine: ranked {} tickers  top={} ({:.1})",
            scores.len(),
            scores.first().map(|s| s.ticker.as_str()).unwrap_or("-"),
            scores.first().map(|s| s.composite).unwrap_or(0.0),
        );

        Ok(scores)
    }

    // ── Signal computation ────────────────────────────────────────────────────

    fn score(&self, data: &MarketData, pairs: Option<&PairsSignal>) -> SignalScore {
        let macro_on = MacroFilter::compute_macro_on(&data.macro_snapshot);

        let momentum_raw    = MomentumSignal.compute(&data.ticker, data);
        let fundamental_raw = FundamentalSignal.compute(&data.ticker, data);
        let insider_raw     = InsiderSignal.compute(&data.ticker, data);
        let sentiment_raw   = SentimentSignal.compute(&data.ticker, data);
        let pairs_raw       = pairs.map(|ps| ps.compute(&data.ticker, data)).unwrap_or(0.0);

        composite_score(
            &data.ticker,
            &data.industry_name,
            momentum_raw,
            fundamental_raw,
            insider_raw,
            sentiment_raw,
            pairs_raw,
            macro_on,
            &self.weights,
        )
    }

    // ── Data gathering ────────────────────────────────────────────────────────

    /// Build a complete `MarketData` bundle for one ticker.
    async fn build_market_data(
        &self,
        ticker: &str,
        industry_name: &str,
        as_of: NaiveDate,
        peer_returns: &HashMap<String, f64>,
        peer_fundamentals: &HashMap<String, FundamentalSnapshot>,
        macro_snapshot: MacroSnapshot,
    ) -> Result<MarketData> {
        // Price history: 13 months for 12-1m momentum + 200-day MA
        let price_from = as_of - Duration::days(400);
        let price_bars = self
            .yahoo
            .price_history(ticker, price_from, as_of)
            .await
            .unwrap_or_default();

        // Fundamentals (point-in-time)
        let fundamentals = self.yahoo.fundamentals(ticker, as_of).await.ok();

        // Insider trades: last 90 days
        let insider_trades = self
            .edgar
            .fetch_insider_trades(ticker, as_of, 90)
            .await
            .unwrap_or_default();

        // News: last 30 days
        let news_items = self
            .gdelt
            .fetch_news_sentiment(ticker, as_of, 30)
            .await
            .unwrap_or_default();

        // Reddit: last 7 days
        let reddit_snapshots = self
            .reddit
            .fetch_reddit_mentions(ticker, as_of, 7)
            .await
            .unwrap_or_default();

        Ok(MarketData {
            ticker: ticker.to_string(),
            as_of,
            industry_name: industry_name.to_string(),
            price_bars,
            fundamentals,
            peer_returns_12m1m: peer_returns.clone(),
            peer_fundamentals: peer_fundamentals.clone(),
            insider_trades,
            news_items,
            reddit_snapshots,
            macro_snapshot,
        })
    }

    /// Pre-fetch peer returns and fundamentals for all tickers in one industry.
    /// Returns (peer_returns_12m1m, peer_fundamentals) excluding the current ticker.
    async fn gather_peer_data(
        &self,
        slots: &[crate::universe::builder::CompanySlot],
        as_of: NaiveDate,
    ) -> (HashMap<String, f64>, HashMap<String, FundamentalSnapshot>) {
        let price_from = as_of - Duration::days(400);
        let mut peer_returns: HashMap<String, f64> = HashMap::new();
        let mut peer_fundamentals: HashMap<String, FundamentalSnapshot> = HashMap::new();

        for slot in slots {
            let ticker = &slot.ticker;

            // 12-1m return
            if let Ok(bars) = self.yahoo.price_history(ticker, price_from, as_of).await {
                if let Some(ret) = compute_12m1m_return(&bars) {
                    peer_returns.insert(ticker.clone(), ret);
                }
            }

            // Fundamentals
            if let Ok(fund) = self.yahoo.fundamentals(ticker, as_of).await {
                peer_fundamentals.insert(ticker.clone(), fund);
            }
        }

        (peer_returns, peer_fundamentals)
    }
}

// ── Price return helper ───────────────────────────────────────────────────────

fn compute_12m1m_return(bars: &[crate::data::PriceBar]) -> Option<f64> {
    if bars.len() < 50 {
        return None;
    }
    let skip = 21.min(bars.len() / 10);
    let end_idx = bars.len().saturating_sub(skip + 1);
    let price_end   = bars[end_idx].adj_close;
    let price_start = bars[0].adj_close;
    if price_start <= 0.0 {
        return None;
    }
    Some((price_end - price_start) / price_start)
}
