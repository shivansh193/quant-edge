use axum::{extract::State, http::StatusCode, Json};
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::llm::parse_strategy;
use crate::paper_trading::{PaperPortfolio, PaperTradingEngine};
use crate::server::state::AppState;

#[derive(Debug, Deserialize)]
pub struct PaperInitRequest {
    pub strategy: String,
    pub capital:  f64,
}

#[derive(Debug, Serialize)]
pub struct PaperStatusResponse {
    pub portfolio:        Option<PaperPortfolio>,
    pub total_value:      f64,
    pub total_pnl_pct:    f64,
    pub cash_pct:         f64,
    pub positions_count:  usize,
    pub days_until_rebal: i64,
    pub rebalance_due:    bool,
}

/// GET /api/paper/status — return current paper portfolio without modifying it.
pub async fn status(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let engine = PaperTradingEngine::new(state.cache.clone());
    match engine.load_portfolio() {
        Ok(Some(portfolio)) => {
            let today = Local::now().date_naive();
            let total_value = portfolio.total_value();
            let inception_val = portfolio.cash
                + portfolio.positions.values().map(|p| p.shares * p.entry_price).sum::<f64>();
            let total_pnl_pct = (total_value / inception_val.max(1.0) - 1.0) * 100.0;
            let cash_pct = portfolio.cash / total_value.max(1.0) * 100.0;
            let days_since = portfolio.days_since_rebalance(today);
            let hold_days = portfolio.strategy_spec.holding_period_days as i64;
            let days_until_rebal = (hold_days - days_since).max(0);
            let rebalance_due = portfolio.rebalance_due(today);
            let positions_count = portfolio.positions.len();

            Ok(Json(json!(PaperStatusResponse {
                portfolio: Some(portfolio),
                total_value,
                total_pnl_pct,
                cash_pct,
                positions_count,
                days_until_rebal,
                rebalance_due,
            })))
        }
        Ok(None) => Ok(Json(json!({
            "portfolio": null,
            "total_value": 0.0,
            "total_pnl_pct": 0.0,
            "cash_pct": 100.0,
            "positions_count": 0,
            "days_until_rebal": 0,
            "rebalance_due": false,
        }))),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })))),
    }
}

/// POST /api/paper/init — initialise a new paper portfolio from a strategy prompt.
pub async fn init(
    State(state): State<AppState>,
    Json(req): Json<PaperInitRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let universe_guard = state.universe.read().await;
    let universe = universe_guard.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "Universe not ready" })),
        )
    })?;

    let spec = parse_strategy(&req.strategy)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, Json(json!({ "error": e.to_string() }))))?;

    let today = Local::now().date_naive();
    let engine = PaperTradingEngine::new(state.cache.clone());
    let portfolio = engine
        .init_portfolio(spec, universe, today, req.capital)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))))?;

    Ok(Json(serde_json::to_value(&portfolio).unwrap_or_default()))
}

/// POST /api/paper/update — mark to market and rebalance if due.
pub async fn update(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let universe_guard = state.universe.read().await;
    let universe = universe_guard.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "Universe not ready" })),
        )
    })?;

    let today = Local::now().date_naive();
    let engine = PaperTradingEngine::new(state.cache.clone());
    let portfolio = engine
        .update_or_init_portfolio(universe, today, 100_000.0)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))))?;

    Ok(Json(serde_json::to_value(&portfolio).unwrap_or_default()))
}
