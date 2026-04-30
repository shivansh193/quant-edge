use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

// ── Industry correlation (computed by Phase-5 correlation engine) ─────────────

#[derive(Debug, Clone)]
pub struct IndustryCorrelation {
    pub industry_a: String,
    pub industry_b: String,
    pub correlation: f64,
    pub date: NaiveDate,
    pub window_days: u32,
}

// ── Insider trades (SEC EDGAR Form 4) ────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsiderTrade {
    pub ticker: String,
    pub filing_date: NaiveDate,
    pub trade_date: NaiveDate,
    pub insider_name: String,
    /// "CEO" | "CFO" | "President" | "Officer" | "Director" | "Other"
    pub insider_role: String,
    pub shares: f64,
    /// "A" = acquired/bought, "D" = disposed/sold
    pub transaction_type: String,
}

// ── News sentiment (GDELT) ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsItem {
    pub ticker: String,
    pub article_date: NaiveDate,
    /// GDELT tone: raw value, typically −100 to +100; we normalise later.
    pub tone: f64,
    pub headline: String,
    pub source: String,
}

// ── Reddit mention data ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedditSnapshot {
    pub ticker: String,
    pub fetch_date: NaiveDate,
    pub subreddit: String,
    pub mention_count: u32,
    pub avg_upvote_ratio: f64,
    pub total_comments: u32,
}

// ── FRED macro data ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MacroDataPoint {
    pub series_id: String,
    pub date: NaiveDate,
    pub value: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MacroSnapshot {
    pub as_of: NaiveDate,
    /// CBOE VIX index level
    pub vix: Option<f64>,
    /// 10-Year Treasury yield (%)
    pub yield_10y: Option<f64>,
    /// 10Y yield 30 calendar days ago (for spike detection)
    pub yield_10y_30d_ago: Option<f64>,
    /// Macro regime gate: false suppresses all long signals
    pub macro_on: bool,
}

impl MacroSnapshot {
    /// Neutral snapshot — used when FRED data is unavailable.
    /// Assumes risk-on so downstream signals are not zeroed out.
    pub fn neutral(as_of: NaiveDate) -> Self {
        Self {
            as_of,
            vix: None,
            yield_10y: None,
            yield_10y_30d_ago: None,
            macro_on: true,
        }
    }
}
