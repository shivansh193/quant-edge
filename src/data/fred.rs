use anyhow::{Context, Result};
use chrono::{Duration, NaiveDate};
use reqwest::{header, Client};
use tracing::{debug, info, warn};

use super::cache::Cache;
use super::types::{MacroDataPoint, MacroSnapshot};

// ── FRED graph CSV endpoint — free, no API key required ──────────────────────
// Cache TTL: 24 hours

const FRED_DELAY_MS: u64 = 500;

/// Series IDs we track
const VIX_SERIES: &str = "VIXCLS";
const YIELD_10Y_SERIES: &str = "GS10";

pub struct FredFetcher {
    client: Client,
    cache: Cache,
}

impl FredFetcher {
    pub fn new(cache: Cache) -> Self {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            header::HeaderValue::from_static("portfolio-sim/0.1"),
        );
        let client = Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("FRED HTTP client build failed");

        Self { client, cache }
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Build a MacroSnapshot for `as_of` date.
    /// Fetches VIX and 10Y yield from FRED (with 24h cache).
    pub async fn macro_snapshot(&self, as_of: NaiveDate) -> MacroSnapshot {
        // Fetch / refresh macro series
        for series_id in &[VIX_SERIES, YIELD_10Y_SERIES] {
            if !self.cache.has_macro_cache(series_id, 24) {
                if let Err(e) = self.refresh_series(series_id).await {
                    warn!(series_id = %series_id, "FRED refresh failed: {:#}", e);
                }
            }
        }

        self.build_snapshot(as_of)
    }

    // ── FRED internals ────────────────────────────────────────────────────────

    async fn refresh_series(&self, series_id: &str) -> Result<()> {
        debug!(series_id = %series_id, "FRED: refreshing series");

        let url = format!(
            "https://fred.stlouisfed.org/graph/fredgraph.csv?id={}",
            series_id
        );

        tokio::time::sleep(std::time::Duration::from_millis(FRED_DELAY_MS)).await;

        let text = self
            .client
            .get(&url)
            .send()
            .await
            .context("FRED request failed")?
            .text()
            .await
            .context("FRED response read failed")?;

        let points = parse_fred_csv(series_id, &text)?;
        info!(series_id = %series_id, n = points.len(), "FRED: cached data points");
        self.cache.insert_macro_data(&points)
    }

    fn build_snapshot(&self, as_of: NaiveDate) -> MacroSnapshot {
        let lookback = as_of - Duration::days(400); // 13 months of history

        let vix = self
            .cache
            .get_macro_data(VIX_SERIES, lookback, as_of)
            .ok()
            .and_then(|pts| last_value(&pts));

        let yield_series = self
            .cache
            .get_macro_data(YIELD_10Y_SERIES, lookback, as_of)
            .unwrap_or_default();

        let yield_10y = last_value(&yield_series);
        let yield_10y_30d_ago = value_n_days_ago(&yield_series, as_of, 30);

        let macro_on = compute_macro_regime(vix, yield_10y, yield_10y_30d_ago);

        MacroSnapshot {
            as_of,
            vix,
            yield_10y,
            yield_10y_30d_ago,
            macro_on,
        }
    }
}

// ── Regime detection ──────────────────────────────────────────────────────────

/// risk_on = VIX < 25 AND 10Y yield not spiking more than 0.5% in 30 days
fn compute_macro_regime(
    vix: Option<f64>,
    yield_now: Option<f64>,
    yield_30d_ago: Option<f64>,
) -> bool {
    // If we can't get data, default to risk-on (neutral)
    let vix_ok = vix.map(|v| v < 25.0).unwrap_or(true);
    let yield_ok = match (yield_now, yield_30d_ago) {
        (Some(now), Some(ago)) => (now - ago) < 0.5,
        _ => true,
    };
    vix_ok && yield_ok
}

// ── FRED CSV parser ───────────────────────────────────────────────────────────

fn parse_fred_csv(series_id: &str, text: &str) -> Result<Vec<MacroDataPoint>> {
    // FRED CSV: first line is header "DATE,VALUE", then data rows
    // Missing values are represented as "."
    let mut points = Vec::new();
    let mut is_header = true;

    for line in text.lines() {
        if is_header {
            is_header = false;
            continue;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, ',');
        let date_str = parts.next().unwrap_or("").trim();
        let value_str = parts.next().unwrap_or("").trim();

        // Skip missing values (".")
        if value_str == "." || value_str.is_empty() {
            continue;
        }

        let Ok(date) = NaiveDate::parse_from_str(date_str, "%Y-%m-%d") else {
            continue;
        };
        let Ok(value) = value_str.parse::<f64>() else {
            continue;
        };

        points.push(MacroDataPoint {
            series_id: series_id.to_string(),
            date,
            value,
        });
    }

    Ok(points)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Most recent value in a chronologically sorted series.
fn last_value(pts: &[MacroDataPoint]) -> Option<f64> {
    pts.last().map(|p| p.value)
}

/// Value closest to `days` calendar days before `as_of`.
fn value_n_days_ago(pts: &[MacroDataPoint], as_of: NaiveDate, days: i64) -> Option<f64> {
    let target = as_of - Duration::days(days);
    // Find the last data point on or before target date
    pts.iter()
        .filter(|p| p.date <= target)
        .last()
        .map(|p| p.value)
}
