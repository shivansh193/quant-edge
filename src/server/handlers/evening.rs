use axum::{extract::State, http::StatusCode, Json};
use chrono::Local;
use serde::Serialize;
use serde_json::{json, Value};

use crate::data::yahoo::YahooFinance;
use crate::paper_trading::PaperTradingEngine;
use crate::server::state::AppState;

#[derive(Debug, Serialize)]
pub struct PositionEod {
    pub ticker:        String,
    pub entry_price:   f64,
    pub current_price: f64,
    pub shares:        f64,
    pub days_held:     i64,
    pub pnl_pct:       f64,
    pub today_move:    f64,
    pub flagged:       bool,
}

#[derive(Debug, Serialize)]
pub struct EveningResponse {
    pub date:            String,
    pub total_value:     f64,
    pub total_pnl_pct:   f64,
    pub positions:       Vec<PositionEod>,
    pub best_today:      Option<(String, f64)>,
    pub worst_today:     Option<(String, f64)>,
    pub flagged_count:   usize,
}

/// GET /api/evening — mark-to-market all open paper positions.
pub async fn evening(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let today = Local::now().date_naive();
    let engine = PaperTradingEngine::new(state.cache.clone());
    let yahoo  = YahooFinance::new(state.cache.clone());

    let portfolio = match engine.load_portfolio() {
        Ok(Some(p)) => p,
        Ok(None) => return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "No paper portfolio found — call POST /api/paper/init first" })),
        )),
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })))),
    };

    let inception_val = portfolio.cash
        + portfolio.positions.values().map(|p| p.shares * p.entry_price).sum::<f64>();

    let mut eod_positions: Vec<PositionEod> = Vec::new();

    for (ticker, pos) in &portfolio.positions {
        let (_, latest) = yahoo.fetch_today_bar(ticker, today).await;
        let prev = state.cache
            .get_price_bars(ticker, today - chrono::Duration::days(7), today - chrono::Duration::days(1))
            .unwrap_or_default()
            .last()
            .map(|b| b.adj_close)
            .unwrap_or(0.0);

        let current_price = if latest > 0.0 { latest } else { pos.current_price };
        let pnl_pct = (current_price - pos.entry_price) / pos.entry_price.max(1e-9) * 100.0;
        let today_move = if prev > 0.0 && current_price > 0.0 {
            (current_price - prev) / prev * 100.0
        } else { 0.0 };
        let flagged = today_move.abs() > 3.0;
        let days_held = (today - pos.entry_date).num_days();

        eod_positions.push(PositionEod {
            ticker: ticker.clone(),
            entry_price: pos.entry_price,
            current_price,
            shares: pos.shares,
            days_held,
            pnl_pct,
            today_move,
            flagged,
        });
    }

    eod_positions.sort_by(|a, b| b.today_move.partial_cmp(&a.today_move).unwrap_or(std::cmp::Ordering::Equal));

    let total_value = portfolio.cash
        + eod_positions.iter().map(|p| p.shares * p.current_price).sum::<f64>();
    let total_pnl_pct = (total_value / inception_val.max(1.0) - 1.0) * 100.0;
    let flagged_count = eod_positions.iter().filter(|p| p.flagged).count();
    let best_today = eod_positions.first().map(|p| (p.ticker.clone(), p.today_move));
    let worst_today = eod_positions.last().map(|p| (p.ticker.clone(), p.today_move));

    let response = EveningResponse {
        date: today.to_string(),
        total_value,
        total_pnl_pct,
        positions: eod_positions,
        best_today,
        worst_today,
        flagged_count,
    };

    // Save JSON log
    std::fs::create_dir_all("logs").ok();
    let log_path = format!("logs/evening_{}.json", today.format("%Y%m%d"));
    std::fs::write(&log_path, serde_json::to_string_pretty(&response).unwrap_or_default()).ok();

    Ok(Json(serde_json::to_value(&response).unwrap_or_default()))
}
