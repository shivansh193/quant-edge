use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use chrono::Local;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::data::{edgar::EdgarFetcher, fred::FredFetcher, gdelt::GdeltFetcher,
                  reddit::RedditFetcher, yahoo::YahooFinance, DataSource};
use crate::server::state::AppState;
use crate::signals::{
    composite_score,
    fundamental::FundamentalSignal,
    insider::InsiderSignal,
    macro_filter::MacroFilter,
    momentum::MomentumSignal,
    sentiment::SentimentSignal,
    MarketData, Signal, SignalAvailability, SignalWeights,
};

#[derive(Debug, Serialize)]
pub struct SignalDetailResponse {
    pub ticker:          String,
    pub date:            String,
    pub composite:       f64,
    pub momentum_raw:    f64,
    pub fundamental_raw: f64,
    pub insider_raw:     f64,
    pub sentiment_raw:   f64,
    pub macro_on:        bool,
    /// How many of the 5 signals had real input data (pairs is not computed here).
    pub signals_available: usize,
    /// Signals that had no usable data and were excluded from `composite`.
    pub missing_signals: Vec<&'static str>,
    pub fundamentals:    Option<FundamentalsDetail>,
    pub insider_count:   usize,
    pub news_count:      usize,
    pub reddit_mentions: u32,
    pub price_1m_pct:    Option<f64>,
    pub price_3m_pct:    Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct FundamentalsDetail {
    pub revenue_ttm:      Option<f64>,
    pub revenue_cagr_3yr: Option<f64>,
    pub net_margin_pct:   Option<f64>,
    pub debt_to_equity:   Option<f64>,
    pub price_to_book:    Option<f64>,
}

/// GET /api/signals/:ticker — full signal breakdown for one ticker.
pub async fn signal_detail(
    State(state): State<AppState>,
    Path(ticker): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let today = Local::now().date_naive();
    let ticker = ticker.to_uppercase();

    let yahoo  = YahooFinance::new(state.cache.clone());
    let edgar  = EdgarFetcher::new(state.cache.clone());
    let gdelt  = GdeltFetcher::new(state.cache.clone());
    let reddit = RedditFetcher::new(state.cache.clone());
    let fred   = FredFetcher::new(state.cache.clone());

    let price_from = today - chrono::Duration::days(400);
    let price_bars = yahoo.price_history(&ticker, price_from, today).await.unwrap_or_default();
    let fundamentals = yahoo.fundamentals(&ticker, today).await.ok();
    let insider_trades = edgar.fetch_insider_trades(&ticker, today, 90).await.unwrap_or_default();
    let news_items = gdelt.fetch_news_sentiment(&ticker, today, 30).await.unwrap_or_default();
    let reddit_snapshots = reddit.fetch_reddit_mentions(&ticker, today, 7).await.unwrap_or_default();
    let macro_snapshot = fred.macro_snapshot(today).await;

    let data = MarketData {
        ticker: ticker.clone(),
        as_of: today,
        industry_name: String::new(),
        price_bars: price_bars.clone(),
        fundamentals: fundamentals.clone(),
        peer_returns_12m1m: HashMap::new(),
        peer_fundamentals: HashMap::new(),
        insider_trades: insider_trades.clone(),
        news_items: news_items.clone(),
        reddit_snapshots: reddit_snapshots.clone(),
        macro_snapshot: macro_snapshot.clone(),
    };

    let macro_on = MacroFilter::compute_macro_on(&macro_snapshot);
    let weights = SignalWeights::default();

    let momentum_raw    = MomentumSignal.compute(&ticker, &data);
    let fundamental_raw = FundamentalSignal.compute(&ticker, &data);
    let insider_raw     = InsiderSignal.compute(&ticker, &data);
    let sentiment_raw   = SentimentSignal.compute(&ticker, &data);

    let availability = SignalAvailability::assess(&data, false);
    let score = composite_score(
        &ticker, "", momentum_raw, fundamental_raw, insider_raw, sentiment_raw, 0.0,
        macro_on, &weights, &availability,
    );

    // Recent price returns
    let price_1m_pct = compute_return(&price_bars, 21);
    let price_3m_pct = compute_return(&price_bars, 63);

    let fundamentals_detail = fundamentals.map(|f| FundamentalsDetail {
        revenue_ttm:      f.revenue_ttm,
        revenue_cagr_3yr: f.revenue_cagr_3yr,
        net_margin_pct:   f.net_margin_pct,
        debt_to_equity:   f.debt_to_equity,
        price_to_book:    f.price_to_book,
    });

    let reddit_mentions: u32 = reddit_snapshots.iter().map(|r| r.mention_count).sum();

    Ok(Json(json!(SignalDetailResponse {
        ticker,
        date: today.to_string(),
        composite: score.composite,
        momentum_raw,
        fundamental_raw,
        insider_raw,
        sentiment_raw,
        macro_on,
        signals_available: availability.count(),
        missing_signals: availability.missing(),
        fundamentals: fundamentals_detail,
        insider_count: insider_trades.len(),
        news_count: news_items.len(),
        reddit_mentions,
        price_1m_pct,
        price_3m_pct,
    })))
}

fn compute_return(bars: &[crate::data::PriceBar], days: usize) -> Option<f64> {
    if bars.len() < days + 1 {
        return None;
    }
    let latest = bars.last()?.adj_close;
    let prior  = bars[bars.len().saturating_sub(days + 1)].adj_close;
    if prior <= 0.0 { return None; }
    Some((latest - prior) / prior * 100.0)
}
