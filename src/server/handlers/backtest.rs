use axum::{extract::State, http::StatusCode, Json};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::backtest::{BacktestConfig, BacktestEngine, BacktestResult};
use crate::llm::{parse_strategy, StrategySpec};
use crate::server::state::AppState;

#[derive(Debug, Deserialize)]
pub struct BacktestRequest {
    /// Natural-language strategy description (parsed via Gemini) OR preset spec JSON.
    pub strategy:   String,
    pub start_date: String,
    pub end_date:   String,
    /// Starting capital (default 100_000).
    pub capital:    Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct BacktestResponse {
    pub strategy_name:   String,
    pub result:          BacktestResult,
    pub ic_mean:         f64,
}

/// POST /api/backtest — run a historical backtest with a natural-language strategy.
pub async fn backtest(
    State(state): State<AppState>,
    Json(req): Json<BacktestRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let universe_guard = state.universe.read().await;
    let universe = universe_guard.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "Universe not ready" })),
        )
    })?;

    let start = NaiveDate::parse_from_str(&req.start_date, "%Y-%m-%d")
        .map_err(|_| (StatusCode::BAD_REQUEST, Json(json!({ "error": "Invalid start_date — use YYYY-MM-DD" }))))?;
    let end = NaiveDate::parse_from_str(&req.end_date, "%Y-%m-%d")
        .map_err(|_| (StatusCode::BAD_REQUEST, Json(json!({ "error": "Invalid end_date — use YYYY-MM-DD" }))))?;

    if start >= end {
        return Err((StatusCode::BAD_REQUEST, Json(json!({ "error": "start_date must be before end_date" }))));
    }

    let spec: StrategySpec = parse_strategy(&req.strategy)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, Json(json!({ "error": e.to_string() }))))?;

    let mut config = BacktestConfig::new(spec.clone(), start, end);
    if let Some(cap) = req.capital {
        config.initial_capital = cap;
    }

    let engine = BacktestEngine::new(state.cache.clone());
    let result = engine
        .run(universe, &config)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))))?;

    let ic_mean = if result.signal_ic_per_period.is_empty() {
        0.0
    } else {
        result.signal_ic_per_period.iter().sum::<f64>() / result.signal_ic_per_period.len() as f64
    };

    // Persist to leaderboard
    let spec_json = serde_json::to_string(&spec).unwrap_or_default();
    let _ = state.cache.save_strategy_run(
        &spec.name,
        &spec_json,
        result.total_return_pct,
        result.sharpe_ratio,
        result.max_drawdown_pct,
        0.0, // alpha not computed in backtest engine
        ic_mean,
        &req.start_date,
        &req.end_date,
    );

    Ok(Json(json!(BacktestResponse {
        strategy_name: spec.name,
        result,
        ic_mean,
    })))
}
