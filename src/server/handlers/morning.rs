use axum::{extract::State, http::StatusCode, Json};
use chrono::Local;
use serde::Serialize;
use serde_json::{json, Value};

use crate::data::{cache::Cache, yahoo::YahooFinance, FredFetcher};
use crate::paper_trading::PaperTradingEngine;
use crate::server::state::AppState;
use crate::signals::{PickingEngine, SignalScore};
use crate::universe::{AutoUniverseBuilder, UniverseBuilder, UniverseConfig, Market, CapFilter};

#[derive(Debug, Serialize)]
pub struct MarketIndex {
    pub label:       String,
    pub price:       f64,
    pub change_5d:   f64,
}

#[derive(Debug, Serialize)]
pub struct MacroContext {
    pub vix:         Option<f64>,
    pub yield_10y:   Option<f64>,
    pub macro_on:    bool,
    pub usdinr:      f64,
    pub indices:     Vec<MarketIndex>,
}

#[derive(Debug, Serialize)]
pub struct PortfolioSnapshot {
    pub total_value:   f64,
    pub total_pnl_pct: f64,
    pub positions:     usize,
    pub rebalance_due: bool,
    pub days_until_rebal: i64,
}

#[derive(Debug, Serialize)]
pub struct MorningResponse {
    pub date:               String,
    pub regime:             String,
    pub us_picks:           Vec<SignalScore>,
    pub in_picks:           Vec<SignalScore>,
    pub macro_context:      MacroContext,
    pub paper_portfolio:    Option<PortfolioSnapshot>,
    pub universe_size:      usize,
    pub score_threshold:    f64,
    pub timestamp:          String,
}

/// GET /api/morning — full morning workflow returning structured JSON.
pub async fn morning(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let today = Local::now().date_naive();
    let threshold = std::env::var("MIN_SCORE_THRESHOLD")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60.0_f64);

    // Build auto universe
    let auto_builder = AutoUniverseBuilder::new(state.cache.clone());
    let tickers = auto_builder
        .get_all_tickers()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))))?;

    let source = YahooFinance::new(state.cache.clone());
    let ub = UniverseBuilder::new(&source, &state.taxonomy);
    let config = UniverseConfig {
        market: Market::Both,
        cap_filter: CapFilter::Mixed,
        n_industries: 100,
        exclude_industry_codes: vec![],
    };
    let mut universe = ub
        .build_from_tickers(tickers, config)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))))?;
    ub.enrich_gics(&mut universe).await.ok();
    universe.trim_to_n_industries(100, &[]);

    let universe_size = universe.total_companies();

    // Run picking engine
    let engine = PickingEngine::new(state.cache.clone());
    let scores = engine
        .rank_universe(&universe, today)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))))?;

    // Score-based top-N
    let picks = crate::daily::score_based_picks(&scores, threshold);
    crate::forward_test::record_best_effort(&state.cache, today, "morning: default composite", &scores, &picks);
    let us_picks: Vec<SignalScore> = picks.iter().filter(|s| !s.ticker.ends_with(".NS")).take(10).cloned().collect();
    let in_picks: Vec<SignalScore> = picks.iter().filter(|s| s.ticker.ends_with(".NS")).take(10).cloned().collect();

    // Read the regime from the full ranking, NOT from `picks`: in risk-off the
    // macro gate returns no picks at all, and defaulting an empty list to
    // "risk-on" would report the opposite of reality exactly when we are in cash.
    let macro_on = scores.first().map(|s| s.macro_on).unwrap_or(true);
    let regime = if macro_on { "Risk-On" } else { "Risk-Off" };

    // Macro context
    let macro_context = build_macro_context(&state.cache, today).await;

    // Paper portfolio snapshot
    let paper_portfolio = build_portfolio_snapshot(&state.cache, today);

    let response = MorningResponse {
        date: today.to_string(),
        regime: regime.to_string(),
        us_picks,
        in_picks,
        macro_context,
        paper_portfolio,
        universe_size,
        score_threshold: threshold,
        timestamp: chrono::Utc::now().to_rfc3339(),
    };

    // Save plain-text log
    let _log_line = format!(
        "[{}] Morning scan: {} tickers, regime={}, US_picks={}, IN_picks={}",
        today,
        universe_size,
        regime,
        response.us_picks.len(),
        response.in_picks.len(),
    );
    std::fs::create_dir_all("logs").ok();
    let log_path = format!("logs/morning_{}.json", today.format("%Y%m%d"));
    std::fs::write(&log_path, serde_json::to_string_pretty(&response).unwrap_or_default()).ok();

    Ok(Json(serde_json::to_value(&response).unwrap_or_default()))
}

async fn build_macro_context(cache: &Cache, today: chrono::NaiveDate) -> MacroContext {
    use chrono::Duration;
    let yahoo = YahooFinance::new(cache.clone());
    let fred  = FredFetcher::new(cache.clone());
    let macro_snap = fred.macro_snapshot(today).await;

    let five_days_ago = today - Duration::days(7);
    let mut indices = Vec::new();

    for (label, ticker) in &[("Nifty50", "^NSEI"), ("S&P500", "^GSPC")] {
        let (_, latest) = yahoo.fetch_today_bar(ticker, today).await;
        let prior = cache
            .get_price_bars(ticker, five_days_ago, today - Duration::days(1))
            .unwrap_or_default()
            .last()
            .map(|b| b.adj_close)
            .unwrap_or(0.0);
        let change_5d = if latest > 0.0 && prior > 0.0 {
            (latest - prior) / prior * 100.0
        } else {
            0.0
        };
        indices.push(MarketIndex { label: label.to_string(), price: latest, change_5d });
    }

    let (_, usdinr) = yahoo.fetch_today_bar("USDINR=X", today).await;
    let macro_on = crate::signals::macro_filter::MacroFilter::compute_macro_on(&macro_snap);

    MacroContext {
        vix:       macro_snap.vix,
        yield_10y: macro_snap.yield_10y,
        macro_on,
        usdinr,
        indices,
    }
}

fn build_portfolio_snapshot(cache: &Cache, today: chrono::NaiveDate) -> Option<PortfolioSnapshot> {
    let engine = PaperTradingEngine::new(cache.clone());
    let portfolio = engine.load_portfolio().ok()??;
    let total_value = portfolio.total_value();
    let inception_val = portfolio.cash
        + portfolio.positions.values().map(|p| p.shares * p.entry_price).sum::<f64>();
    let total_pnl_pct = (total_value / inception_val.max(1.0) - 1.0) * 100.0;
    let days_since = portfolio.days_since_rebalance(today);
    let hold_days = portfolio.strategy_spec.holding_period_days as i64;
    let days_until_rebal = (hold_days - days_since).max(0);
    Some(PortfolioSnapshot {
        total_value,
        total_pnl_pct,
        positions: portfolio.positions.len(),
        rebalance_due: portfolio.rebalance_due(today),
        days_until_rebal,
    })
}
