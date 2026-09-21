pub mod state;
pub mod presets;
pub mod handlers;

use axum::{
    routing::{get, post},
    Router,
};
use tower_http::{
    cors::{Any, CorsLayer},
    services::ServeDir,
};

use state::AppState;

/// Build the Axum router with all API routes and static file serving.
pub fn build_router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let api = Router::new()
        .route("/health",              get(handlers::health::health))
        .route("/picks",               post(handlers::picks::picks))
        .route("/backtest",            post(handlers::backtest::backtest))
        .route("/strategy/parse",      post(handlers::strategy::parse))
        .route("/strategies/presets",  get(handlers::strategy::list_presets))
        .route("/strategies/run",      post(handlers::strategy::run_preset))
        .route("/strategies/history",  get(handlers::portfolio::strategy_history))
        .route("/paper/status",        get(handlers::paper::status))
        .route("/paper/init",          post(handlers::paper::init))
        .route("/paper/update",        post(handlers::paper::update))
        .route("/morning",             get(handlers::morning::morning))
        .route("/evening",             get(handlers::evening::evening))
        .route("/signals/:ticker",     get(handlers::signals::signal_detail))
        .route("/correlations",        get(handlers::correlations::correlations))
        .route("/universe",            get(handlers::universe::universe))
        .route("/portfolio/history",   get(handlers::portfolio::history))
        .with_state(state);

    // Serve Next.js static export (production) — `npm run build` outputs to frontend/out/.
    // Falls back to legacy dashboard/ if frontend/out/ doesn't exist yet.
    let static_dir = if std::path::Path::new("frontend/out").exists() {
        "frontend/out"
    } else {
        "dashboard"
    };

    Router::new()
        .nest("/api", api)
        .nest_service("/", ServeDir::new(static_dir).append_index_html_on_directories(true))
        .layer(cors)
}
