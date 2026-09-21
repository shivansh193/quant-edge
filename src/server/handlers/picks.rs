use axum::{extract::State, http::StatusCode, Json};
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::llm::{parse_strategy, StrategySpec};
use crate::signals::{PickingEngine, SignalScore};
use crate::server::state::AppState;

#[derive(Debug, Deserialize)]
pub struct PicksRequest {
    /// "US", "IN", or "both" — filters results by ticker suffix (.NS = IN).
    pub market:   Option<String>,
    /// Natural-language strategy; if provided, Gemini parses it first.
    pub strategy: Option<String>,
    pub top_n:    Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct PicksResponse {
    pub date:    String,
    pub picks:   Vec<SignalScore>,
    pub regime:  String,
}

/// POST /api/picks — score and rank the loaded universe.
pub async fn picks(
    State(state): State<AppState>,
    Json(req): Json<PicksRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let universe_guard = state.universe.read().await;
    let universe = universe_guard.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "Universe not ready — server is still initialising" })),
        )
    })?;

    let today = Local::now().date_naive();

    let spec: Option<StrategySpec> = if let Some(ref text) = req.strategy {
        Some(
            parse_strategy(text)
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, Json(json!({ "error": e.to_string() }))))?,
        )
    } else {
        None
    };

    let mut engine = PickingEngine::new(state.cache.clone());
    if let Some(ref s) = spec {
        engine.apply_strategy_spec(s);
    }

    let mut scores = engine
        .rank_universe(universe, today)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))))?;

    // Market filter
    if let Some(ref market) = req.market {
        let market_lc = market.to_lowercase();
        scores = match market_lc.as_str() {
            "in" | "nse" => scores.into_iter().filter(|s| s.ticker.ends_with(".NS")).collect(),
            "us" | "nyse" => scores.into_iter().filter(|s| !s.ticker.ends_with(".NS")).collect(),
            _ => scores,
        };
    }

    if let Some(n) = req.top_n {
        scores.truncate(n);
    }

    let regime = scores
        .first()
        .map(|s| if s.macro_on { "Risk-On" } else { "Risk-Off" })
        .unwrap_or("Unknown")
        .to_string();

    Ok(Json(json!(PicksResponse {
        date: today.to_string(),
        picks: scores,
        regime,
    })))
}
