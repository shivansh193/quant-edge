use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::{Duration, NaiveDate};
use reqwest::{Client, header};
use serde::Deserialize;
use std::collections::HashMap;
use tokio::sync::Mutex;

use super::asof::AsOf;
use super::cache::Cache;
use super::prices::momentum_12m1m;
use super::sec_facts::SecFactsFetcher;
use super::source::{AssetInfo, DataSource, FundamentalSnapshot, MarketCap, PriceBar};

// ── Yahoo Finance JSON shapes (v8 chart API) ─────────────────────────────────

#[derive(Deserialize)]
struct YfChartResponse {
    chart: YfChart,
}

#[derive(Deserialize)]
struct YfChart {
    result: Option<Vec<YfResult>>,
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct YfResult {
    timestamp: Vec<i64>,
    indicators: YfIndicators,
}

#[derive(Deserialize)]
struct YfIndicators {
    quote: Vec<YfQuote>,
    #[serde(rename = "adjclose")]
    adjclose: Option<Vec<YfAdjClose>>,
}

#[derive(Deserialize)]
struct YfQuote {
    open: Vec<Option<f64>>,
    high: Vec<Option<f64>>,
    low: Vec<Option<f64>>,
    close: Vec<Option<f64>>,
    volume: Vec<Option<u64>>,
}

#[derive(Deserialize)]
struct YfAdjClose {
    adjclose: Vec<Option<f64>>,
}

// ── Yahoo Finance summary (for fundamentals) ──────────────────────────────────

#[derive(Deserialize)]
struct YfQuoteSummaryResponse {
    #[serde(rename = "quoteSummary")]
    quote_summary: YfQuoteSummary,
}

#[derive(Deserialize)]
struct YfQuoteSummary {
    result: Option<Vec<HashMap<String, serde_json::Value>>>,
    error: Option<serde_json::Value>,
}

// ── Crumb state (Yahoo requires a session cookie + crumb for v10) ─────────────

struct YahooCrumb {
    cookie: String,
    crumb:  String,
}

// ── Implementation ────────────────────────────────────────────────────────────

pub struct YahooFinance {
    client: Client,
    cache:  Cache,
    crumb:  Mutex<Option<YahooCrumb>>,
    sec:    SecFactsFetcher,
}

impl YahooFinance {
    pub fn new(cache: Cache) -> Self {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            header::HeaderValue::from_static(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
                 AppleWebKit/537.36 (KHTML, like Gecko) \
                 Chrome/120.0.0.0 Safari/537.36",
            ),
        );
        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static(
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            ),
        );
        headers.insert(
            header::ACCEPT_LANGUAGE,
            header::HeaderValue::from_static("en-US,en;q=0.5"),
        );

        Self {
            client: Client::builder()
                .default_headers(headers)
                .cookie_store(true)
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .expect("HTTP client construction failed"),
            sec: SecFactsFetcher::new(cache.clone()),
            cache,
            crumb: Mutex::new(None),
        }
    }

    fn to_epoch(date: NaiveDate) -> i64 {
        date.and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp()
    }

    fn classify_cap(cap_usd: f64) -> MarketCap {
        match cap_usd {
            c if c < 2_000_000_000.0  => MarketCap::SmallCap,
            c if c < 10_000_000_000.0 => MarketCap::MidCap,
            _                          => MarketCap::LargeCap,
        }
    }

    /// Obtain a Yahoo session cookie + crumb (required since 2024).
    /// Fetches once per process lifetime and reuses.
    async fn ensure_crumb(&self) -> Result<(String, String)> {
        let mut guard = self.crumb.lock().await;
        if let Some(ref c) = *guard {
            return Ok((c.cookie.clone(), c.crumb.clone()));
        }

        // Step 1: hit the consent/home page to get cookies
        let _consent = self
            .client
            .get("https://finance.yahoo.com/")
            .send()
            .await
            .context("Yahoo consent page failed")?;

        // Small delay to appear human
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Step 2: fetch crumb
        let crumb_resp = self
            .client
            .get("https://query1.finance.yahoo.com/v1/test/getcrumb")
            .header("Referer", "https://finance.yahoo.com/")
            .send()
            .await
            .context("Yahoo crumb fetch failed")?;

        let status = crumb_resp.status();
        let crumb_text = crumb_resp.text().await.unwrap_or_default();

        if !status.is_success() || crumb_text.is_empty() || crumb_text.contains("error") {
            return Err(anyhow!(
                "Yahoo crumb fetch returned status={} body={}",
                status,
                &crumb_text[..crumb_text.len().min(200)]
            ));
        }

        let crumb = crumb_text.trim().to_string();
        tracing::debug!("Yahoo crumb acquired: {}", &crumb[..crumb.len().min(8)]);

        // We don't need to store cookie separately — reqwest's cookie_store handles it
        let cookie = String::new();
        *guard = Some(YahooCrumb { cookie: cookie.clone(), crumb: crumb.clone() });

        Ok((cookie, crumb))
    }

    /// Fetch raw price bars from Yahoo v8 chart API (no crumb needed)
    async fn fetch_bars_remote(
        &self,
        ticker: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<PriceBar>> {
        let url = format!(
            "https://query1.finance.yahoo.com/v8/finance/chart/{}?\
             period1={}&period2={}&interval=1d&includeAdjustedClose=true",
            ticker,
            Self::to_epoch(from),
            Self::to_epoch(to + Duration::days(1)),
        );

        // Small delay to avoid rate limits
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let resp: YfChartResponse = self
            .client
            .get(&url)
            .header("Referer", "https://finance.yahoo.com/")
            .send()
            .await
            .context("Yahoo chart request failed")?
            .json()
            .await
            .context("Failed to parse Yahoo chart JSON")?;

        let result = resp
            .chart
            .result
            .and_then(|mut r| r.pop())
            .ok_or_else(|| anyhow!("No chart data for {}: {:?}", ticker, resp.chart.error))?;

        let quote = result
            .indicators
            .quote
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Empty quote indicators for {}", ticker))?;

        let adjclose_series: Vec<Option<f64>> = result
            .indicators
            .adjclose
            .and_then(|mut a| a.pop())
            .map(|a| a.adjclose)
            .unwrap_or_default();

        let bars: Vec<PriceBar> = result
            .timestamp
            .iter()
            .enumerate()
            .filter_map(|(i, &ts)| {
                let open     = quote.open.get(i)?.as_ref()?;
                let high     = quote.high.get(i)?.as_ref()?;
                let low      = quote.low.get(i)?.as_ref()?;
                let close    = quote.close.get(i)?.as_ref()?;
                let volume   = quote.volume.get(i)?.as_ref()?;
                let adj_close = adjclose_series
                    .get(i)
                    .and_then(|v| v.as_ref())
                    .copied()
                    .unwrap_or(*close);

                let date = chrono::DateTime::from_timestamp(ts, 0)?.date_naive();

                Some(PriceBar {
                    date,
                    open: *open,
                    high: *high,
                    low: *low,
                    close: *close,
                    adj_close,
                    volume: *volume,
                })
            })
            .collect();

        Ok(bars)
    }

    /// Fetch quote summary modules from Yahoo v10 (requires crumb)
    async fn fetch_summary_modules(
        &self,
        ticker: &str,
        modules: &[&str],
    ) -> Result<HashMap<String, serde_json::Value>> {
        let (_cookie, crumb) = self.ensure_crumb().await?;
        let modules_str = modules.join(",");
        let url = format!(
            "https://query1.finance.yahoo.com/v10/finance/quoteSummary/{}?\
             modules={}&crumb={}",
            ticker, modules_str, crumb
        );

        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        let raw = self
            .client
            .get(&url)
            .header("Referer", "https://finance.yahoo.com/")
            .send()
            .await
            .context("Yahoo quoteSummary request failed")?
            .text()
            .await
            .context("Failed to read Yahoo quoteSummary body")?;

        // Parse with helpful error showing the actual body
        let resp: YfQuoteSummaryResponse = serde_json::from_str(&raw).map_err(|e| {
            anyhow!(
                "Failed to parse Yahoo quoteSummary JSON for {}: {}\nBody: {}",
                ticker,
                e,
                &raw[..raw.len().min(300)]
            )
        })?;

        if let Some(ref err) = resp.quote_summary.error {
            return Err(anyhow!("Yahoo quoteSummary error for {}: {}", ticker, err));
        }

        let result = resp
            .quote_summary
            .result
            .and_then(|mut r| r.pop())
            .ok_or_else(|| anyhow!("Empty quoteSummary result for {}", ticker))?;

        Ok(result)
    }

    fn extract_raw(val: &serde_json::Value) -> Option<f64> {
        val.get("raw")?.as_f64()
    }

    /// Force-fetch today's bar from Yahoo, bypassing the cache.
    /// Returns (today_open, today_close/current). Falls back to cached last
    /// close on network failure.
    pub async fn fetch_today_bar(&self, ticker: &str, today: NaiveDate) -> (f64, f64) {
        let from = today - Duration::days(5);
        match self.fetch_bars_remote(ticker, from, today).await {
            Ok(bars) if !bars.is_empty() => {
                let _ = self.cache.insert_price_bars(ticker, &bars);
                let last = bars.last().unwrap();
                let open = if last.date == today {
                    last.open
                } else {
                    last.close
                };
                (open, last.adj_close)
            }
            _ => {
                // Network failed — return cached data
                let cached = self.cache
                    .get_price_bars(ticker, from, today)
                    .unwrap_or_default();
                let last = cached.last();
                let close = last.map(|b| b.adj_close).unwrap_or(0.0);
                let open  = last.map(|b| b.open).unwrap_or(close);
                (open, close)
            }
        }
    }
}

#[async_trait]
impl DataSource for YahooFinance {
    async fn price_history(
        &self,
        ticker: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<PriceBar>> {
        if self.cache.has_price_coverage(ticker, from, to) {
            return self.cache.get_price_bars(ticker, from, to);
        }
        let bars = self.fetch_bars_remote(ticker, from, to).await?;
        self.cache.insert_price_bars(ticker, &bars)?;
        Ok(bars)
    }

    async fn asset_info(&self, ticker: &str) -> Result<AssetInfo> {
        if let Some(info) = self.cache.get_asset_info(ticker)? {
            return Ok(info);
        }

        let data = self
            .fetch_summary_modules(ticker, &["assetProfile", "summaryDetail", "price"])
            .await?;

        let price_module = data.get("price").cloned().unwrap_or_default();
        let asset_profile = data.get("assetProfile").cloned().unwrap_or_default();
        let gics_industry: Option<String> = asset_profile
            .get("industry")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let market_cap_raw = price_module
            .get("marketCap")
            .and_then(Self::extract_raw);

        let info = AssetInfo {
            ticker: ticker.to_string(),
            name: price_module
                .get("longName")
                .or_else(|| price_module.get("shortName"))
                .and_then(|v| v.as_str())
                .unwrap_or(ticker)
                .to_string(),
            exchange: price_module
                .get("exchangeName")
                .and_then(|v| v.as_str())
                .unwrap_or("UNKNOWN")
                .to_string(),
            currency: price_module
                .get("currency")
                .and_then(|v| v.as_str())
                .unwrap_or("USD")
                .to_string(),
            gics_industry: gics_industry,
            market_cap_usd: market_cap_raw,
            market_cap_tier: market_cap_raw.map(Self::classify_cap),
        };

        self.cache.insert_asset_info(&info)?;
        Ok(info)
    }

    async fn fundamentals(
        &self,
        ticker: &str,
        as_of: NaiveDate,
    ) -> Result<FundamentalSnapshot> {
        let view = AsOf::new(&self.cache, as_of);

        // 1. Cached, and only if its provenance is valid for this date.
        if let Some(snap) = view.fundamentals(ticker)? {
            return Ok(snap);
        }

        // 2. Point-in-time from dated SEC filings (US issuers). Correct for any date.
        match self.sec.snapshot(ticker, as_of).await {
            Ok(mut snap) => {
                snap.price_return_12m_1m = view
                    .price_bars(ticker, 400)
                    .ok()
                    .and_then(|bars| momentum_12m1m(&bars));
                self.cache.insert_fundamentals(&snap, "sec_pit")?;
                return Ok(snap);
            }
            Err(e) => tracing::debug!(ticker = %ticker, "no SEC point-in-time fundamentals: {e:#}"),
        }

        // 3. Yahoo only ever returns TODAY's numbers. That is honest for a live
        //    date and a look-ahead leak for any other — so refuse the latter
        //    instead of stamping current data with an old date (the old bug).
        if view.is_historical() {
            return Err(anyhow!(
                "no point-in-time fundamentals for {ticker} as of {as_of}; \
                 Yahoo's current snapshot would leak the future"
            ));
        }

        let data = self
            .fetch_summary_modules(
                ticker,
                &["financialData", "defaultKeyStatistics", "summaryDetail"],
            )
            .await?;

        let fin   = data.get("financialData").cloned().unwrap_or_default();
        let stats = data.get("defaultKeyStatistics").cloned().unwrap_or_default();

        let revenue_ttm       = fin.get("totalRevenue").and_then(Self::extract_raw);
        let net_margin_pct    = fin.get("profitMargins").and_then(Self::extract_raw).map(|v| v * 100.0);
        let debt_to_equity    = fin.get("debtToEquity").and_then(Self::extract_raw);
        let price_to_book     = stats.get("priceToBook").and_then(Self::extract_raw);
        let operating_cashflow = fin.get("operatingCashflow").and_then(Self::extract_raw);
        let return_on_assets  = fin.get("returnOnAssets").and_then(Self::extract_raw).map(|v| v * 100.0);
        let gross_profit_margin = fin.get("grossProfits")
            .and_then(Self::extract_raw)
            .and_then(|gp| revenue_ttm.filter(|&rev| rev > 0.0).map(|rev| gp / rev * 100.0));

        let snap = FundamentalSnapshot {
            ticker: ticker.to_string(),
            date: as_of,
            revenue_ttm,
            revenue_cagr_3yr: None,
            net_margin_pct,
            debt_to_equity,
            price_to_book,
            price_return_12m_1m: view
                .price_bars(ticker, 400)
                .ok()
                .and_then(|bars| momentum_12m1m(&bars)),
            market_share_proxy: None,
            operating_cashflow,
            return_on_assets,
            gross_profit_margin,
        };

        self.cache.insert_fundamentals(&snap, "yahoo_current")?;
        Ok(snap)
    }
}
