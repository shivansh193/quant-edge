use anyhow::{anyhow, Context, Result};
use chrono::{Duration, NaiveDate};
use reqwest::{header, Client};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::{debug, warn};

use super::cache::Cache;
use super::types::InsiderTrade;

// ── EDGAR rate limit: ≤10 req/sec; use 150ms between calls ──────────────────

const EDGAR_DELAY_MS: u64 = 150;
const EDGAR_USER_AGENT: &str = "portfolio-sim research@example.com";

/// Fetches Form 4 insider-trade filings from SEC EDGAR.
/// All results are cached in SQLite with a 48-hour TTL.
pub struct EdgarFetcher {
    client: Client,
    cache: Cache,
    /// In-memory ticker→CIK map; populated lazily on first use.
    cik_map: Arc<Mutex<HashMap<String, u64>>>,
}

impl EdgarFetcher {
    pub fn new(cache: Cache) -> Self {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            header::HeaderValue::from_static(EDGAR_USER_AGENT),
        );
        headers.insert(
            header::ACCEPT_ENCODING,
            header::HeaderValue::from_static("gzip, deflate"),
        );
        let client = Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("EDGAR HTTP client build failed");

        Self {
            client,
            cache,
            cik_map: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Return insider trades for `ticker` within `days` days ending at `as_of`.
    /// Fetches from EDGAR if cache is stale (> 48 hours old).
    pub async fn fetch_insider_trades(
        &self,
        ticker: &str,
        as_of: NaiveDate,
        days: u32,
    ) -> Result<Vec<InsiderTrade>> {
        let from = as_of - Duration::days(days as i64);

        // Return cached data if fresh (48h TTL)
        if self.cache.has_insider_cache(ticker, 48) {
            return self.cache.get_insider_trades(ticker, from, as_of);
        }

        debug!(ticker = %ticker, "EDGAR: fetching Form 4 filings");

        let trades = match self.fetch_from_edgar(ticker, days).await {
            Ok(t) => t,
            Err(e) => {
                warn!(ticker = %ticker, "EDGAR fetch failed: {:#}", e);
                // Return whatever is in cache (may be stale but better than nothing)
                return self.cache.get_insider_trades(ticker, from, as_of);
            }
        };

        if let Err(e) = self.cache.insert_insider_trades(&trades) {
            warn!(ticker = %ticker, "EDGAR cache write failed: {:#}", e);
        }

        self.cache.get_insider_trades(ticker, from, as_of)
    }

    // ── EDGAR internals ───────────────────────────────────────────────────────

    async fn fetch_from_edgar(&self, ticker: &str, days: u32) -> Result<Vec<InsiderTrade>> {
        let cik = self.get_cik(ticker).await?;
        let accessions = self.get_form4_accessions(cik, days).await?;

        let mut all_trades: Vec<InsiderTrade> = Vec::new();
        for (accession, filing_date, primary_doc) in accessions {
            tokio::time::sleep(std::time::Duration::from_millis(EDGAR_DELAY_MS)).await;
            match self
                .fetch_form4_xml(cik, &accession, &primary_doc, ticker, filing_date)
                .await
            {
                Ok(mut trades) => all_trades.append(&mut trades),
                Err(e) => {
                    warn!(ticker = %ticker, filing = %accession, "Form 4 parse failed: {:#}", e);
                }
            }
        }

        Ok(all_trades)
    }

    /// Resolve ticker → CIK using EDGAR's company_tickers.json.
    async fn get_cik(&self, ticker: &str) -> Result<u64> {
        let upper = ticker.to_uppercase();

        // Check in-memory cache first
        {
            let guard = self.cik_map.lock().unwrap();
            if let Some(&cik) = guard.get(&upper) {
                return Ok(cik);
            }
        }

        // Fetch the full ticker→CIK map from EDGAR (once per process)
        tokio::time::sleep(std::time::Duration::from_millis(EDGAR_DELAY_MS)).await;
        let raw: serde_json::Value = self
            .client
            .get("https://www.sec.gov/files/company_tickers.json")
            .send()
            .await
            .context("EDGAR company_tickers fetch failed")?
            .json()
            .await
            .context("EDGAR company_tickers parse failed")?;

        let mut guard = self.cik_map.lock().unwrap();

        if let Some(obj) = raw.as_object() {
            for entry in obj.values() {
                if let (Some(t), Some(c)) = (
                    entry.get("ticker").and_then(|v| v.as_str()),
                    entry.get("cik_str").and_then(|v| v.as_u64()),
                ) {
                    guard.insert(t.to_uppercase(), c);
                }
            }
        }

        guard
            .get(&upper)
            .copied()
            .ok_or_else(|| anyhow!("CIK not found for ticker {}", ticker))
    }

    /// Fetch the list of recent Form 4 accession numbers for a CIK.
    /// Returns (accession_number, filing_date, primary_document).
    async fn get_form4_accessions(
        &self,
        cik: u64,
        days: u32,
    ) -> Result<Vec<(String, NaiveDate, String)>> {
        let padded = format!("{:010}", cik);
        let url = format!(
            "https://data.sec.gov/submissions/CIK{}.json",
            padded
        );

        tokio::time::sleep(std::time::Duration::from_millis(EDGAR_DELAY_MS)).await;

        let raw: serde_json::Value = self
            .client
            .get(&url)
            .send()
            .await
            .context("EDGAR submissions fetch failed")?
            .json()
            .await
            .context("EDGAR submissions parse failed")?;

        let recent = raw
            .pointer("/filings/recent")
            .ok_or_else(|| anyhow!("No filings.recent in EDGAR submissions"))?;

        let forms = recent
            .get("form")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("No form array in EDGAR filings"))?;

        let dates = recent
            .get("filingDate")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("No filingDate array"))?;

        let accessions = recent
            .get("accessionNumber")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("No accessionNumber array"))?;

        let docs = recent
            .get("primaryDocument")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("No primaryDocument array"))?;

        let cutoff = chrono::Local::now().date_naive() - Duration::days(days as i64);

        let mut result = Vec::new();
        for i in 0..forms.len() {
            let form = forms.get(i).and_then(|v| v.as_str()).unwrap_or("");
            if form != "4" {
                continue;
            }
            let date_str = dates.get(i).and_then(|v| v.as_str()).unwrap_or("");
            let Ok(date) = NaiveDate::parse_from_str(date_str, "%Y-%m-%d") else {
                continue;
            };
            if date < cutoff {
                continue;
            }
            let acc = accessions
                .get(i)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let doc = docs
                .get(i)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            if !acc.is_empty() && !doc.is_empty() {
                result.push((acc, date, doc));
            }
        }

        Ok(result)
    }

    /// Fetch and parse one Form 4 XML document.
    async fn fetch_form4_xml(
        &self,
        cik: u64,
        accession: &str,
        primary_doc: &str,
        ticker: &str,
        filing_date: NaiveDate,
    ) -> Result<Vec<InsiderTrade>> {
        let acc_nodash = accession.replace('-', "");
        let url = format!(
            "https://www.sec.gov/Archives/edgar/data/{}/{}/{}",
            cik, acc_nodash, primary_doc
        );

        let xml = self
            .client
            .get(&url)
            .send()
            .await
            .context("Form 4 XML fetch failed")?
            .text()
            .await
            .context("Form 4 XML read failed")?;

        Ok(parse_form4_xml(ticker, filing_date, &xml))
    }
}

// ── Form 4 XML parser (string-based, no extra dependency) ───────────────────

fn parse_form4_xml(ticker: &str, filing_date: NaiveDate, xml: &str) -> Vec<InsiderTrade> {
    let mut trades = Vec::new();

    // Extract insider identity
    let (insider_name, insider_role) = extract_insider_info(xml);

    // Parse all nonDerivativeTransaction blocks
    for block in xml_all_sections(xml, "nonDerivativeTransaction") {
        let trade_date = xml_nested_val(&block, "transactionDate", "value")
            .and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok());

        let shares = xml_nested_val(&block, "transactionShares", "value")
            .and_then(|s| s.parse::<f64>().ok());

        let tx_type = xml_nested_val(&block, "transactionAcquiredDisposedCode", "value");

        if let (Some(td), Some(sh), Some(tt)) = (trade_date, shares, tx_type) {
            // Only record open-market buys/sells (code P=purchase, S=sale)
            // Exclude grants/awards (G, A codes are often compensation, not discretionary)
            let tx_code = xml_nested_val(&block, "transactionCode", "value")
                .unwrap_or_default();
            if matches!(tx_code.as_str(), "P" | "S") || matches!(tt.as_str(), "A" | "D") {
                trades.push(InsiderTrade {
                    ticker: ticker.to_string(),
                    filing_date,
                    trade_date: td,
                    insider_name: insider_name.clone(),
                    insider_role: insider_role.clone(),
                    shares: sh,
                    transaction_type: tt,
                });
            }
        }
    }

    trades
}

fn extract_insider_info(xml: &str) -> (String, String) {
    let owner_section = xml_section(xml, "reportingOwner").unwrap_or_default();

    let name = xml_val(&owner_section, "rptOwnerName")
        .unwrap_or_else(|| "Unknown".to_string());

    let is_officer = xml_val(&owner_section, "isOfficer")
        .map(|v| v.trim() == "1")
        .unwrap_or(false);
    let is_director = xml_val(&owner_section, "isDirector")
        .map(|v| v.trim() == "1")
        .unwrap_or(false);
    let officer_title = xml_val(&owner_section, "officerTitle")
        .unwrap_or_default()
        .to_uppercase();

    let role = if officer_title.contains("CEO") || officer_title.contains("CHIEF EXECUTIVE") {
        "CEO"
    } else if officer_title.contains("CFO") || officer_title.contains("CHIEF FINANCIAL") {
        "CFO"
    } else if officer_title.contains("PRESIDENT") {
        "President"
    } else if officer_title.contains("COO") || officer_title.contains("CHIEF OPERATING") {
        "COO"
    } else if is_officer {
        "Officer"
    } else if is_director {
        "Director"
    } else {
        "Other"
    };

    (name, role.to_string())
}

// ── Minimal XML helpers ───────────────────────────────────────────────────────
// No dependency — works for the predictable Form 4 structure.

/// Return text content of the first `<tag>...</tag>` found in `xml`.
fn xml_val(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let s = xml.find(&open)? + open.len();
    let e = s + xml[s..].find(&close)?;
    Some(xml[s..e].trim().to_string())
}

/// Return the content block inside `<outer_tag>...</outer_tag>`.
/// Handles both `<tag>` and `<tag attr="...">` forms.
fn xml_section(xml: &str, tag: &str) -> Option<String> {
    let open_bare = format!("<{}>", tag);
    let open_attr = format!("<{} ", tag);
    let close = format!("</{}>", tag);

    let tag_start = xml
        .find(&open_bare)
        .or_else(|| xml.find(&open_attr))?;
    let gt = xml[tag_start..].find('>')? + tag_start + 1;
    let end = gt + xml[gt..].find(&close)?;
    Some(xml[gt..end].to_string())
}

/// Within `xml`, find `<outer_tag>`, then find `<inner_tag>` inside it,
/// and return its text value.
fn xml_nested_val(xml: &str, outer_tag: &str, inner_tag: &str) -> Option<String> {
    let section = xml_section(xml, outer_tag)?;
    xml_val(&section, inner_tag)
}

/// Return all occurrences of content inside `<tag>...</tag>`.
fn xml_all_sections(xml: &str, tag: &str) -> Vec<String> {
    let open_bare = format!("<{}>", tag);
    let open_attr = format!("<{} ", tag);
    let close = format!("</{}>", tag);
    let mut result = Vec::new();
    let mut pos = 0usize;

    loop {
        // Find the next opening tag
        let a = xml[pos..].find(&open_bare).map(|i| pos + i);
        let b = xml[pos..].find(&open_attr).map(|i| pos + i);
        let tag_start = match (a, b) {
            (Some(x), Some(y)) => x.min(y),
            (Some(x), None) | (None, Some(x)) => x,
            (None, None) => break,
        };

        let Some(gt_rel) = xml[tag_start..].find('>') else {
            break;
        };
        let content_start = tag_start + gt_rel + 1;

        let Some(close_rel) = xml[content_start..].find(&close) else {
            break;
        };
        let content_end = content_start + close_rel;

        result.push(xml[content_start..content_end].to_string());
        pos = content_end + close.len();
    }

    result
}
