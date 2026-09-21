use axum::{extract::State, http::StatusCode, Json};
use chrono::Local;
use serde_json::{json, Value};

use crate::server::state::AppState;
use crate::signals::PickingEngine;

/// GET /api/universe — score the full universe and return all tickers with scores.
pub async fn universe(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let universe_guard = state.universe.read().await;
    let uni = universe_guard.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "Universe not ready" })),
        )
    })?;

    let today = Local::now().date_naive();
    let engine = PickingEngine::new(state.cache.clone());
    let scores = engine
        .rank_universe(uni, today)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))))?;

    Ok(Json(json!({
        "date":   today.to_string(),
        "count":  scores.len(),
        "tickers": scores,
    })))
}
