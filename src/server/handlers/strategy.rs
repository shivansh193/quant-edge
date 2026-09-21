use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::llm::parse_strategy;
use crate::server::presets::{all_presets, find_preset, AI_PICKS_PROMPT};
use crate::server::state::AppState;
use crate::signals::PickingEngine;

#[derive(Debug, Deserialize)]
pub struct ParseRequest {
    pub prompt: String,
}

#[derive(Debug, Deserialize)]
pub struct RunPresetRequest {
    pub preset_id: String,
}

/// POST /api/strategy/parse — call Gemini, return structured StrategySpec.
pub async fn parse(
    Json(req): Json<ParseRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let spec = parse_strategy(&req.prompt)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, Json(json!({ "error": e.to_string() }))))?;
    Ok(Json(serde_json::to_value(&spec).unwrap_or_default()))
}

/// GET /api/strategies/presets — list all 12 preset strategies.
pub async fn list_presets() -> Json<Value> {
    Json(json!(all_presets()))
}

/// POST /api/strategies/run — run a preset strategy through the picking engine.
pub async fn run_preset(
    State(state): State<AppState>,
    Json(req): Json<RunPresetRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let universe_guard = state.universe.read().await;
    let universe = universe_guard.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "Universe not ready" })),
        )
    })?;

    let mut preset = find_preset(&req.preset_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("Unknown preset: {}", req.preset_id) })),
        )
    })?;

    // ai_picks delegates spec generation to Gemini
    if req.preset_id == "ai_picks" {
        preset.spec = parse_strategy(AI_PICKS_PROMPT)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, Json(json!({ "error": e.to_string() }))))?;
    }

    let today = chrono::Local::now().date_naive();
    let mut engine = PickingEngine::new(state.cache.clone());
    engine.apply_strategy_spec(&preset.spec);

    let mut scores = engine
        .rank_universe(universe, today)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))))?;

    scores.truncate(preset.spec.top_n);

    let regime = scores
        .first()
        .map(|s| if s.macro_on { "Risk-On" } else { "Risk-Off" })
        .unwrap_or("Unknown");

    Ok(Json(json!({
        "preset": preset,
        "date": today.to_string(),
        "regime": regime,
        "picks": scores,
    })))
}
