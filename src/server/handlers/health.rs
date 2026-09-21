use axum::Json;
use serde_json::{json, Value};

/// GET /api/health — liveness check.
pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}
