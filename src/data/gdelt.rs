use anyhow::{Context, Result};
use chrono::NaiveDate;
use reqwest::{header, Client};
use tracing::{debug, warn};

use super::asof::{is_historical, AsOf};
use super::cache::Cache;
use super::types::NewsItem;

// ── GDELT v2 Doc API — free, no key required ─────────────────────────────────
// Endpoint: https://api.gdeltproject.org/api/v2/doc/doc
// Cache TTL: 6 hours

const GDELT_DELAY_MS: u64 = 500;

/// Why live news fetching is off. Found by testing against the real API:
///   * ArtList articles carry NO `tone` field, so the old `unwrap_or(0.0)` made
///     every article a perfectly neutral zero: the signal held no information;
///   * GDELT allows one request per 5 seconds and answers throttled requests
///     with plain text, which the parser then failed on (every request, in
///     practice, for a 400+ ticker universe);
///   * ticker symbols are poor keyword queries.
/// A working version needs GDELT's TimelineTone mode, company-name queries and
/// a shortlist rather than the whole universe. Until then the signal reports
/// "no data" and is excluded from the composite instead of pretending.
pub const NEWS_DISABLED_REASON: &str =
    "GDELT news sentiment is disabled: article-list mode has no tone and the API allows 1 request / 5 s";

/// Flip to true only after the TimelineTone redesign described above.
const NEWS_FETCH_DISABLED: bool = true;

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
        let view = AsOf::new(&self.cache, as_of);

        // Serve only what is already stored (articles carry their own dates).
        // Never fetch: see NEWS_DISABLED_REASON. Historical dates could not be
        // fetched anyway (GDELT's timespan is relative to *now*).
        if NEWS_FETCH_DISABLED || is_historical(as_of) || self.cache.has_news_cache(ticker, 6) {
            return view.news_items(ticker, days as i64);
        }

        debug!(ticker = %ticker, "GDELT: fetching news sentiment");

        let items = match self.fetch_from_gdelt(ticker, days).await {
            Ok(i) => i,
            Err(e) => {
                warn!(ticker = %ticker, "GDELT fetch failed: {:#}", e);
                return view.news_items(ticker, days as i64);
            }
        };

        if let Err(e) = self.cache.insert_news_items(&items) {
            warn!(ticker = %ticker, "GDELT cache write failed: {:#}", e);
        }

        view.news_items(ticker, days as i64)
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
        // No tone means no sentiment information: skip the article rather than
        // record a fabricated neutral 0.0.
        let Some(tone) = article.get("tone").and_then(|v| v.as_f64()) else {
            continue;
        };

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
