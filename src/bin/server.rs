use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;

use quant_edge::{
    data::{cache::Cache, yahoo::YahooFinance},
    gics::GicsTaxonomy,
    server::{build_router, state::AppState},
    universe::{UniverseBuilder, UniverseConfig, Market, CapFilter},
};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "quant_edge=info,warn".into()),
        ))
        .with_target(false)
        .compact()
        .init();

    let cache_path    = std::env::var("CACHE_FILE").unwrap_or_else(|_| "cache.db".into());
    let gics_path     = std::env::var("GICS_FILE").unwrap_or_else(|_| "data/gics.csv".into());
    let tickers_file  = std::env::var("TICKERS_FILE").unwrap_or_else(|_| "tickers.txt".into());
    let port: u16     = std::env::var("SERVER_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);

    let cache = Cache::open(&cache_path)
        .with_context(|| format!("Cannot open cache at {}", cache_path))?;

    let taxonomy = GicsTaxonomy::load(&gics_path)
        .with_context(|| format!("Cannot load GICS taxonomy from {}", gics_path))?;

    let universe_slot: Arc<RwLock<Option<Arc<quant_edge::universe::builder::Universe>>>> =
        Arc::new(RwLock::new(None));

    let state = AppState {
        cache:        cache.clone(),
        taxonomy:     Arc::new(taxonomy),
        universe:     universe_slot.clone(),
        tickers_file: tickers_file.clone(),
        gics_path:    gics_path.clone(),
    };

    // Spawn universe build in background so the server starts immediately
    {
        let cache2    = cache.clone();
        let taxonomy2 = state.taxonomy.clone();
        let slot2     = universe_slot.clone();
        let tf        = tickers_file.clone();

        tokio::spawn(async move {
            tracing::info!("Building universe from {} …", tf);
            let source = YahooFinance::new(cache2.clone());
            let ub     = UniverseBuilder::new(&source, &taxonomy2);
            let config = UniverseConfig {
                market:                 Market::Both,
                cap_filter:             CapFilter::Mixed,
                n_industries:           50,
                exclude_industry_codes: vec![],
            };

            match ub.build_from_file(&tf, config).await {
                Ok(mut uni) => {
                    ub.enrich_gics(&mut uni).await.ok();
                    uni.trim_to_n_industries(50, &[]);
                    let n = uni.total_companies();
                    let mut guard = slot2.write().await;
                    *guard = Some(Arc::new(uni));
                    tracing::info!("Universe ready — {} companies", n);
                }
                Err(e) => tracing::error!("Universe build failed: {:#}", e),
            }
        });
    }

    let router = build_router(state);
    let addr   = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("Cannot bind to {}", addr))?;

    tracing::info!("quant-edge-server listening on http://{}", addr);
    tracing::info!("Dashboard: http://localhost:{}", port);
    tracing::info!("Health:    http://localhost:{}/api/health", port);

    axum::serve(listener, router).await?;
    Ok(())
}
