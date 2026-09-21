use anyhow::{Context, Result};
use chrono::NaiveDate;
use reqwest::{header, Client};
use tracing::{debug, warn};

use super::asof::{is_historical, AsOf};
use super::cache::Cache;
use super::types::RedditSnapshot;

// ── Reddit public search API — no auth for read-only search ──────────────────
// Cache TTL: 12 hours (Reddit data is volatile, refresh often)

const REDDIT_DELAY_MS: u64 = 1_000; // Reddit rate limit: 1 req/sec for anon
const SUBREDDITS: &[&str] = &["wallstreetbets", "stocks"];

pub struct RedditFetcher {
    client: Client,
    cache: Cache,
}

impl RedditFetcher {
    pub fn new(cache: Cache) -> Self {
        let mut headers = header::HeaderMap::new();
        // Reddit requires a proper User-Agent or returns 429
        headers.insert(
            header::USER_AGENT,
            header::HeaderValue::from_static(
                "Mozilla/5.0 portfolio-sim/0.1 (research use)",
            ),
        );
        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static("application/json"),
        );

        let client = Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("Reddit HTTP client build failed");

        Self { client, cache }
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Return Reddit snapshots for `ticker` from the last `days` days.
    /// Aggregates across r/wallstreetbets and r/stocks.
    /// Cache TTL: 12 hours.
    pub async fn fetch_reddit_mentions(
        &self,
        ticker: &str,
        as_of: NaiveDate,
        days: u32,
    ) -> Result<Vec<RedditSnapshot>> {
        let view = AsOf::new(&self.cache, as_of);

        // Reddit search only reflects the present. A snapshot is only valid on
        // the day it was taken, so for a past date serve stored snapshots only.
        if is_historical(as_of) || self.cache.has_reddit_cache(ticker, 12) {
            return view.reddit_snapshots(ticker, days as i64);
        }

        debug!(ticker = %ticker, "Reddit: fetching mentions");

        let mut any_ok = false;
        for &sub in SUBREDDITS {
            tokio::time::sleep(std::time::Duration::from_millis(REDDIT_DELAY_MS)).await;
            match self.fetch_subreddit(ticker, sub, as_of, days).await {
                Ok(snap) => {
                    if let Err(e) = self.cache.insert_reddit_snapshot(&snap) {
                        warn!(ticker = %ticker, "Reddit cache write: {:#}", e);
                    }
                    any_ok = true;
                }
                Err(e) => {
                    warn!(ticker = %ticker, subreddit = %sub, "Reddit fetch: {:#}", e);
                }
            }
        }

        if !any_ok {
            warn!(ticker = %ticker, "All Reddit subreddit fetches failed");
        }

        view.reddit_snapshots(ticker, days as i64)
    }

    // ── Reddit internals ──────────────────────────────────────────────────────

    async fn fetch_subreddit(
        &self,
        ticker: &str,
        subreddit: &str,
        as_of: NaiveDate,
        days: u32,
    ) -> Result<RedditSnapshot> {
        let timeframe = if days <= 1 {
            "day"
        } else if days <= 7 {
            "week"
        } else {
            "month"
        };

        let url = format!(
            "https://www.reddit.com/r/{}/search.json\
             ?q={}&sort=new&restrict_sr=1&t={}&limit=100",
            subreddit, ticker, timeframe
        );

        let text = self
            .client
            .get(&url)
            .send()
            .await
            .context("Reddit request failed")?
            .text()
            .await
            .context("Reddit response read failed")?;

        parse_reddit_response(ticker, as_of, subreddit, &text)
    }
}

// ── Reddit response parser ────────────────────────────────────────────────────

fn parse_reddit_response(
    ticker: &str,
    as_of: NaiveDate,
    subreddit: &str,
    text: &str,
) -> Result<RedditSnapshot> {
    let json: serde_json::Value =
        serde_json::from_str(text).context("Reddit JSON parse failed")?;

    let children = json
        .pointer("/data/children")
        .and_then(|v| v.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[]);

    let mut mention_count = 0u32;
    let mut total_comments = 0u32;
    let mut upvote_sum = 0.0f64;
    let mut upvote_n = 0u32;

    for child in children {
        let data = match child.get("data") {
            Some(d) => d,
            None => continue,
        };

        // Count every post returned as a mention
        mention_count += 1;

        let comments = data
            .get("num_comments")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        total_comments += comments;

        if let Some(ratio) = data.get("upvote_ratio").and_then(|v| v.as_f64()) {
            upvote_sum += ratio;
            upvote_n += 1;
        }
    }

    let avg_upvote_ratio = if upvote_n > 0 {
        upvote_sum / upvote_n as f64
    } else {
        0.5 // neutral default
    };

    Ok(RedditSnapshot {
        ticker: ticker.to_string(),
        fetch_date: as_of,
        subreddit: subreddit.to_string(),
        mention_count,
        avg_upvote_ratio,
        total_comments,
    })
}
