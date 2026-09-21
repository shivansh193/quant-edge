use axum::{extract::State, http::StatusCode, Json};
use chrono::Local;
use serde_json::{json, Value};

use crate::server::state::AppState;

/// GET /api/portfolio/history — 30-day rolling P&L history for paper portfolio.
pub async fn history(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // Load the paper portfolio and reconstruct a 30-day rolling value series
    // from the daily price bars stored in cache.
    use crate::paper_trading::PaperTradingEngine;

    let engine = PaperTradingEngine::new(state.cache.clone());
    let portfolio = match engine.load_portfolio() {
        Ok(Some(p)) => p,
        Ok(None) => return Ok(Json(json!({ "history": [], "message": "No portfolio" }))),
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })))),
    };

    let today = Local::now().date_naive();
    let from = today - chrono::Duration::days(30);

    // Build daily value series by summing position values per day
    let mut daily: std::collections::BTreeMap<chrono::NaiveDate, f64> = std::collections::BTreeMap::new();

    for (ticker, pos) in &portfolio.positions {
        let bars = state.cache
            .get_price_bars(ticker, from, today)
            .unwrap_or_default();
        for bar in &bars {
            let day_value = pos.shares * bar.adj_close;
            *daily.entry(bar.date).or_insert(0.0) += day_value;
        }
    }

    let history: Vec<Value> = daily
        .iter()
        .map(|(date, &pos_value)| {
            json!({
                "date": date.to_string(),
                "value": pos_value + portfolio.cash,
            })
        })
        .collect();

    Ok(Json(json!({ "history": history })))
}

/// GET /api/strategies/history — leaderboard of all past backtest runs.
pub async fn strategy_history(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let rows = state.cache
        .load_strategy_history()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))))?;

    Ok(Json(json!({ "runs": rows })))
}
