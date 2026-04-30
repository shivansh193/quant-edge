use anyhow::{Context, Result};
use chrono::{Duration, NaiveDate};
use reqwest::{header, Client};
use tracing::{debug, warn};

use super::cache::Cache;
use super::types::NewsItem;

// ── GDELT v2 Doc API — free, no key required ─────────────────────────────────
// Endpoint: https://api.gdeltproject.org/api/v2/doc/doc
// Cache TTL: 6 hours

const GDELT_DELAY_MS: u64 = 500;

pub struct GdeltFetcher {
    client: Client,
    cache: Cache,
}

impl GdeltFetcher {
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
            .expect("GDELT HTTP client build failed");

        Self { client, cache }
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Fetch news sentiment for `ticker` over the last `days` days.
    /// Returns cached data (6h TTL) when available.
    pub async fn fetch_news_sentiment(
        &self,
        ticker: &str,
        as_of: NaiveDate,
        days: u32,
    ) -> Result<Vec<NewsItem>> {
        let from = as_of - Duration::days(days as i64);

        if self.cache.has_news_cache(ticker, 6) {
            return self.cache.get_news_items(ticker, from, as_of);
        }

        debug!(ticker = %ticker, "GDELT: fetching news sentiment");

        let items = match self.fetch_from_gdelt(ticker, days).await {
            Ok(i) => i,
            Err(e) => {
                warn!(ticker = %ticker, "GDELT fetch failed: {:#}", e);
                return self.cache.get_news_items(ticker, from, as_of);
            }
        };

        if let Err(e) = self.cache.insert_news_items(&items) {
            warn!(ticker = %ticker, "GDELT cache write failed: {:#}", e);
        }

        self.cache.get_news_items(ticker, from, as_of)
    }

    // ── GDELT internals ───────────────────────────────────────────────────────

    async fn fetch_from_gdelt(&self, ticker: &str, days: u32) -> Result<Vec<NewsItem>> {
        let timespan = format!("{}d", days.min(90)); // GDELT caps at 90d
        let query = format!("\"{}\" sourceCountry:US", ticker);
        let url = format!(
            "https://api.gdeltproject.org/api/v2/doc/doc\
             ?query={query}&mode=ArtList&maxrecords=250&timespan={timespan}&format=json",
            query = urlencoded(&query),
            timespan = timespan,
        );

        tokio::time::sleep(std::time::Duration::from_millis(GDELT_DELAY_MS)).await;

        let text = self
            .client
            .get(&url)
            .send()
            .await
            .context("GDELT request failed")?
            .text()
            .await
            .context("GDELT response read failed")?;

        parse_gdelt_response(ticker, &text)
    }
}

// ── GDELT response parser ─────────────────────────────────────────────────────

fn parse_gdelt_response(ticker: &str, text: &str) -> Result<Vec<NewsItem>> {
    // GDELT sometimes returns JSONP or malformed JSON — be resilient
    let cleaned = text.trim();
    if cleaned.is_empty() || cleaned.starts_with('<') {
        return Ok(Vec::new());
    }

    let json: serde_json::Value = serde_json::from_str(cleaned)
        .context("GDELT JSON parse failed")?;

    let articles = match json.get("articles").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Ok(Vec::new()),
    };

    let mut items = Vec::new();

    for article in articles {
        let tone = article
            .get("tone")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);

        // GDELT seendate format: "20240115T120000Z" or "2024-01-15T12:00:00Z"
        let date_str = article
            .get("seendate")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let article_date = parse_gdelt_date(date_str).unwrap_or_else(|| chrono::Local::now().date_naive());

        let headline = article
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let source = article
            .get("domain")
            .or_else(|| article.get("source"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        items.push(NewsItem {
            ticker: ticker.to_string(),
            article_date,
            tone,
            headline,
            source,
        });
    }

    Ok(items)
}

fn parse_gdelt_date(s: &str) -> Option<NaiveDate> {
    // Try "20240115T120000Z"
    if s.len() >= 8 && !s.contains('-') {
        let ymd = &s[..8];
        return NaiveDate::parse_from_str(ymd, "%Y%m%d").ok();
    }
    // Try "2024-01-15T..." or "2024-01-15"
    let date_part = s.split('T').next()?;
    NaiveDate::parse_from_str(date_part, "%Y-%m-%d").ok()
}

fn urlencoded(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            ' ' => '+'.to_string(),
            other => format!("%{:02X}", other as u32),
        })
        .collect()
}
