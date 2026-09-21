use axum::{extract::State, http::StatusCode, Json};
use chrono::Local;
use serde::Serialize;
use serde_json::{json, Value};

use crate::correlations::CorrelationEngine;
use crate::server::state::AppState;

#[derive(Debug, Clone, Serialize)]
pub struct CorrelationPair {
    pub industry_a:  String,
    pub industry_b:  String,
    pub correlation: f64,
}

#[derive(Debug, Serialize)]
pub struct CorrelationsResponse {
    pub date:              String,
    pub pairs:             Vec<CorrelationPair>,
    pub top_correlated:    Vec<CorrelationPair>,
    pub least_correlated:  Vec<CorrelationPair>,
    pub industries:        Vec<String>,
}

/// GET /api/correlations — industry correlation matrix + concentration analysis.
pub async fn correlations(
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
    let industry_tickers = universe.industry_ticker_map();

    let corr_data = CorrelationEngine::new(state.cache.clone())
        .load_or_compute(&industry_tickers, today)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))))?;

    let all_pairs: Vec<CorrelationPair> = corr_data
        .sorted_pairs()
        .iter()
        .map(|((a, b), r)| CorrelationPair {
            industry_a:  a.clone(),
            industry_b:  b.clone(),
            correlation: *r,
        })
        .collect();

    let top_correlated: Vec<CorrelationPair> = all_pairs.iter().take(15).cloned().collect();
    let least_correlated: Vec<CorrelationPair> = all_pairs.iter().rev().take(10).cloned().collect();

    let mut industries: Vec<String> = industry_tickers.keys().cloned().collect();
    industries.sort();

    Ok(Json(json!(CorrelationsResponse {
        date: today.to_string(),
        pairs: all_pairs,
        top_correlated,
        least_correlated,
        industries,
    })))
}
