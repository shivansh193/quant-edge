use std::sync::Arc;
use tokio::sync::RwLock;

use crate::data::cache::Cache;
use crate::gics::GicsTaxonomy;
use crate::universe::builder::Universe;

/// Shared application state injected into every Axum handler.
#[derive(Clone)]
pub struct AppState {
    pub cache:    Cache,
    pub taxonomy: Arc<GicsTaxonomy>,
    /// Universe is built asynchronously at startup; None until ready.
    pub universe: Arc<RwLock<Option<Arc<Universe>>>>,
    pub tickers_file: String,
    pub gics_path:    String,
}
