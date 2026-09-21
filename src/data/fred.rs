use anyhow::{Context, Result};
use chrono::{Duration, NaiveDate};
use reqwest::{header, Client};
use tracing::{debug, info, warn};

use super::cache::Cache;
use super::source::{DataSource, PriceBar};
use super::types::{MacroDataPoint, MacroSnapshot};
use super::yahoo::YahooFinance;

// ── FRED graph CSV endpoint — free, no API key required ──────────────────────
// Cache TTL: 24 hours

const FRED_DELAY_MS: u64 = 500;

/// Series IDs we track
const VIX_SERIES: &str = "VIXCLS";
// DGS10 is the DAILY 10-year yield. (GS10, used before, is a MONTHLY average,
// far too coarse for a "30-day spike" test.)
const YIELD_10Y_SERIES: &str = "DGS10";

/// Yahoo symbols carrying the same information, as daily closes. Yahoo is tried
/// first: it is the same source the rest of the tool depends on, and FRED's CSV
/// endpoint proved unreachable/very slow in testing.
const VIX_YAHOO: &str = "^VIX";
const YIELD_10Y_YAHOO: &str = "^TNX"; // quoted in percent, e.g. 4.25 = 4.25%

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
        // Fetch / refresh macro series: Yahoo first, FRED as the fallback.
        for (series_id, yahoo_symbol) in [(VIX_SERIES, VIX_YAHOO), (YIELD_10Y_SERIES, YIELD_10Y_YAHOO)] {
            if self.cache.has_macro_cache(series_id, 24) {
                continue;
            }
            if let Err(e) = self.refresh_from_yahoo(series_id, yahoo_symbol).await {
                warn!(series_id = %series_id, "Yahoo macro refresh failed: {:#} - trying FRED", e);
                if let Err(e) = self.refresh_series(series_id).await {
                    warn!(series_id = %series_id, "FRED refresh failed too: {:#}", e);
                }
            }
        }

        let snapshot = self.build_snapshot(as_of);
        if snapshot.vix.is_none() && snapshot.yield_10y.is_none() {
            // Fail-open is the documented default, but it must never be silent:
            // before this warning existed the macro gate ran without any data
            // (the macro_data table was empty) and nothing said so.
            warn!(
                as_of = %as_of,
                "MACRO GATE HAS NO DATA (VIX and 10Y both unavailable): treating every date as risk-on"
            );
        }
        snapshot
    }

    /// Load a daily macro series from Yahoo and store it under `series_id`.
    async fn refresh_from_yahoo(&self, series_id: &str, symbol: &str) -> Result<()> {
        let yahoo = YahooFinance::new(self.cache.clone());
        let from = NaiveDate::from_ymd_opt(2010, 1, 1).expect("valid date");
        let today = chrono::Local::now().date_naive();
        let bars = yahoo
            .price_history(symbol, from, today)
            .await
            .with_context(|| format!("Yahoo history for {symbol} failed"))?;
        let points = bars_to_macro_points(series_id, &bars);
        anyhow::ensure!(!points.is_empty(), "Yahoo returned no usable data for {symbol}");
        info!(series_id = %series_id, source = %symbol, n = points.len(), "macro series cached from Yahoo");
        self.cache.insert_macro_data(&points)
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

/// Daily closes as macro data points; non-finite or non-positive values dropped.
pub fn bars_to_macro_points(series_id: &str, bars: &[PriceBar]) -> Vec<MacroDataPoint> {
    bars.iter()
        .filter(|b| b.close.is_finite() && b.close > 0.0)
        .map(|b| MacroDataPoint { series_id: series_id.to_string(), date: b.date, value: b.close })
        .collect()
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(date: &str, close: f64) -> PriceBar {
        PriceBar { date: date.parse().unwrap(), open: close, high: close, low: close, close, adj_close: close, volume: 0 }
    }

    #[test]
    fn yahoo_closes_become_macro_points_and_bad_values_are_dropped() {
        let pts = bars_to_macro_points(
            "VIXCLS",
            &[bar("2024-01-02", 13.2), bar("2024-01-03", f64::NAN), bar("2024-01-04", 0.0), bar("2024-01-05", 14.1)],
        );
        assert_eq!(pts.len(), 2);
        assert!(pts.iter().all(|p| p.series_id == "VIXCLS"));
        assert_eq!(pts[1].value, 14.1);
    }

    #[test]
    fn regime_flips_on_high_vix_or_a_yield_spike() {
        assert!(compute_macro_regime(Some(18.0), Some(4.0), Some(3.9)));
        assert!(!compute_macro_regime(Some(31.0), Some(4.0), Some(3.9)), "VIX >= 25 is risk-off");
        assert!(!compute_macro_regime(Some(15.0), Some(4.6), Some(4.0)), "+0.6 in 30d is a spike");
        assert!(compute_macro_regime(None, None, None), "no data is fail-open (and now warns loudly)");
    }
}
