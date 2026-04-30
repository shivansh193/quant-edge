use async_trait::async_trait;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use anyhow::Result;

/// One OHLCV bar (daily)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceBar {
    pub date: NaiveDate,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub adj_close: f64,
    pub volume: u64,
}

/// Market cap tier — used for universe filtering
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MarketCap {
    SmallCap,  // < 2B USD
    MidCap,    // 2B–10B USD
    LargeCap,  // > 10B USD
}

/// Basic company info — resolved once, cached
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetInfo {
    pub ticker: String,
    pub name: String,
    pub exchange: String,   // "NSE" | "NYSE" etc.
    pub currency: String,
    pub gics_industry: Option<String>,
    pub market_cap_usd: Option<f64>,
    pub market_cap_tier: Option<MarketCap>,
}

/// Point-in-time fundamentals snapshot for role classification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FundamentalSnapshot {
    pub ticker: String,
    pub date: NaiveDate,

    // Role metrics
    pub revenue_ttm: Option<f64>,         // LargestByRevenue
    pub revenue_cagr_3yr: Option<f64>,    // FastestGrower
    pub net_margin_pct: Option<f64>,      // MostProfitable
    pub debt_to_equity: Option<f64>,      // MostLeveraged
    pub price_to_book: Option<f64>,       // DeepValue
    pub price_return_12m_1m: Option<f64>, // MomentumLeader (12m-1m momentum)
    pub market_share_proxy: Option<f64>,  // ConsumerReach (revenue / industry total)
}

/// The core trait — every data source implements this.
/// Swapping Yahoo → EODHD is a one-line config change.
#[async_trait]
pub trait DataSource: Send + Sync {
    /// Fetch OHLCV bars for a ticker between two dates (inclusive).
    /// Returns cached data if available, otherwise fetches and caches.
    async fn price_history(
        &self,
        ticker: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<PriceBar>>;

    /// Fetch static asset info (name, exchange, GICS, market cap).
    async fn asset_info(&self, ticker: &str) -> Result<AssetInfo>;

    /// Fetch point-in-time fundamentals for role classification.
    /// `as_of` is the rebalancing date — avoids look-ahead bias.
    async fn fundamentals(
        &self,
        ticker: &str,
        as_of: NaiveDate,
    ) -> Result<FundamentalSnapshot>;

    /// Batch version — fetches multiple tickers concurrently.
    /// Default impl fans out; sources can override for bulk endpoints.
    async fn price_history_batch(
        &self,
        tickers: &[String],
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<(String, Vec<PriceBar>)>> {
        use futures::future::join_all;

        let futures: Vec<_> = tickers
            .iter()
            .map(|t| self.price_history(t, from, to))
            .collect();

        let results = join_all(futures).await;

        tickers
            .iter()
            .zip(results)
            .map(|(ticker, res)| res.map(|bars| (ticker.clone(), bars)))
            .collect()
    }
}