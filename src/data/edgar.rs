use anyhow::{anyhow, Context, Result};
use chrono::{Duration, NaiveDate};
use reqwest::{header, Client};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::{debug, warn};

use super::asof::{is_historical, AsOf};
use super::cache::Cache;
use super::types::InsiderTrade;

// ── EDGAR rate limit: ≤10 req/sec; use 150ms between calls ──────────────────

const EDGAR_DELAY_MS: u64 = 150;
const EDGAR_USER_AGENT: &str = "portfolio-sim research@example.com";

/// One ticker->CIK map for the whole process (it is a ~1 MB download).
fn shared_cik_map() -> Arc<Mutex<HashMap<String, u64>>> {
    static MAP: std::sync::OnceLock<Arc<Mutex<HashMap<String, u64>>>> = std::sync::OnceLock::new();
    MAP.get_or_init(|| Arc::new(Mutex::new(HashMap::new()))).clone()
}

/// GET `url` as JSON, retrying up to `attempts` times on any failure (request
/// error, non-2xx status, or an unparsable body) with a short linear backoff.
/// Always waits `EDGAR_DELAY_MS` before each attempt, so this also carries the
/// rate-limit delay rather than needing a separate sleep at each call site.
async fn fetch_json_with_retries<T: serde::de::DeserializeOwned>(
    client: &Client,
    url: &str,
    attempts: u32,
) -> Result<T> {
    let mut last_err = None;
    for attempt in 0..attempts.max(1) {
        tokio::time::sleep(std::time::Duration::from_millis(
            EDGAR_DELAY_MS + attempt as u64 * 500,
        ))
        .await;
        let result: Result<T> = async {
            let resp = client.get(url).send().await.context("request failed")?;
            let resp = resp.error_for_status().context("non-2xx status")?;
            resp.json::<T>().await.context("body was not valid JSON")
        }
        .await;
        match result {
            Ok(v) => return Ok(v),
            Err(e) => {
                warn!(url, attempt = attempt + 1, attempts, "fetch failed, {}: {e:#}",
                      if attempt + 1 < attempts { "retrying" } else { "giving up" });
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("fetch_json_with_retries called with 0 attempts")))
}

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
        // SEC requires a User-Agent that identifies you (name + contact email).
        // Set SEC_USER_AGENT="Your Name you@example.com" in .env; the built-in
        // placeholder works but may be rate-limited or blocked.
        let ua = std::env::var("SEC_USER_AGENT")
            .ok()
            .and_then(|v| header::HeaderValue::from_str(&v).ok())
            .unwrap_or_else(|| header::HeaderValue::from_static(EDGAR_USER_AGENT));
        headers.insert(header::USER_AGENT, ua);
        // Deliberately NO Accept-Encoding header: reqwest is built without gzip
        // support here, and SEC honours the header, so asking for compression
        // returned bytes we could not decode. Every CIK lookup failed, and the
        // callers' unwrap_or_default() hid it - the insider signal was silently
        // empty for every ticker.
        let client = Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("EDGAR HTTP client build failed");

        Self {
            client,
            cache,
            cik_map: shared_cik_map(),
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
        let view = AsOf::new(&self.cache, as_of);

        // A historical date can only be answered from filings we already hold:
        // fetching "recent" filings now would describe the present, not then.
        if is_historical(as_of) {
            return view.insider_trades(ticker, days as i64);
        }

        // Return cached data if fresh (48h TTL)
        if self.cache.has_insider_cache(ticker, 48) {
            return view.insider_trades(ticker, days as i64);
        }

        debug!(ticker = %ticker, "EDGAR: fetching Form 4 filings");

        let trades = match self.fetch_from_edgar(ticker, days).await {
            Ok(t) => t,
            Err(e) => {
                warn!(ticker = %ticker, "EDGAR fetch failed: {:#}", e);
                // Return whatever is in cache (may be stale but better than nothing)
                return view.insider_trades(ticker, days as i64);
            }
        };

        if let Err(e) = self.cache.insert_insider_trades(&trades) {
            warn!(ticker = %ticker, "EDGAR cache write failed: {:#}", e);
        }

        view.insider_trades(ticker, days as i64)
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

    /// Download the XBRL "company facts" document for a ticker.
    pub async fn company_facts(&self, ticker: &str) -> Result<serde_json::Value> {
        let cik = self.get_cik(ticker).await?;
        tokio::time::sleep(std::time::Duration::from_millis(EDGAR_DELAY_MS)).await;
        let url = format!("https://data.sec.gov/api/xbrl/companyfacts/CIK{:010}.json", cik);
        self.client
            .get(&url)
            .send()
            .await
            .context("EDGAR company facts request failed")?
            .error_for_status()
            .context("EDGAR company facts returned an error status")?
            .json()
            .await
            .context("EDGAR company facts parse failed")
    }

    /// Resolve ticker → CIK using EDGAR's company_tickers.json.
    async fn get_cik(&self, ticker: &str) -> Result<u64> {
        let upper = ticker.to_uppercase();

        // Check in-memory cache first. Once the map has been loaded, a ticker
        // that is not in it (e.g. an NSE listing) has no CIK - fail fast rather
        // than re-downloading the whole file on every call.
        {
            let guard = self.cik_map.lock().unwrap();
            if let Some(&cik) = guard.get(&upper) {
                return Ok(cik);
            }
            if !guard.is_empty() {
                return Err(anyhow!("CIK not found for ticker {}", ticker));
            }
        }

        // Fetch the full ticker→CIK map from EDGAR (once per process). This one
        // fetch gates every subsequent ticker's CIK lookup for the rest of the
        // run (the empty-map check above means a single transient failure here
        // makes EVERY ticker retry-and-fail all over again) — worth retrying a
        // couple of times before giving up, unlike a routine per-ticker fetch.
        // Seen in practice: a clean re-run of the identical request a minute
        // later succeeded, consistent with a transient blip rather than a
        // real block (SEC does not otherwise rate-limit this endpoint at our
        // request rate).
        let raw: serde_json::Value = fetch_json_with_retries(
            &self.client,
            "https://www.sec.gov/files/company_tickers.json",
            3,
        )
        .await
        .context("EDGAR company_tickers fetch failed after retries")?;

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
            cik, acc_nodash, raw_form4_filename(primary_doc)
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

/// The submissions index lists Form 4's primary document as the XSL-rendered
/// view (e.g. `xslF345X05/wk-form4_123.xml`), which is HTML. The raw XML lives
/// at the same filename without the `xsl...` directory.
fn raw_form4_filename(primary_doc: &str) -> &str {
    primary_doc.rsplit('/').next().unwrap_or(primary_doc)
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
            // Only open-market purchases (P) and sales (S). Grants (A), option
            // exercises (M), tax withholding (F), gifts (G) etc. are mechanical
            // compensation events, not discretionary views. The previous
            // `|| tt is A/D` clause let all of them through, contradicting the
            // comment that described this filter.
            // transactionCode is a plain text element (<transactionCode>S</...>),
            // unlike the amounts/dates, which wrap their text in <value>.
            let tx_code = xml_val(&block, "transactionCode").unwrap_or_default();
            if matches!(tx_code.as_str(), "P" | "S") {
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
    } else if officer_title.contains("PRESIDENT") && !officer_title.contains("VICE") {
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


#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    /// Structure copied from a real Form 4 (values shortened).
    fn form4(title: &str, txs: &[(&str, &str, &str, &str)]) -> String {
        // (date, code, shares, acquired/disposed)
        let mut body = String::new();
        for (date, code, shares, ad) in txs {
            body.push_str(&format!(
                "<nonDerivativeTransaction>                   <securityTitle><value>Common Stock</value></securityTitle>                   <transactionDate><value>{date}</value></transactionDate>                   <transactionCoding><transactionFormType>4</transactionFormType>                     <transactionCode>{code}</transactionCode><equitySwapInvolved>0</equitySwapInvolved></transactionCoding>                   <transactionAmounts>                     <transactionShares><value>{shares}</value><footnoteId id=\"F1\"/></transactionShares>                     <transactionPricePerShare><value>10</value></transactionPricePerShare>                     <transactionAcquiredDisposedCode><value>{ad}</value></transactionAcquiredDisposedCode>                   </transactionAmounts>                 </nonDerivativeTransaction>"
            ));
        }
        format!(
            "<?xml version=\"1.0\"?><ownershipDocument>               <reportingOwner><reportingOwnerId><rptOwnerCik>1</rptOwnerCik><rptOwnerName>DOE JANE</rptOwnerName></reportingOwnerId>                 <reportingOwnerRelationship><isDirector>0</isDirector><isOfficer>1</isOfficer>                   <officerTitle>{title}</officerTitle></reportingOwnerRelationship></reportingOwner>               <nonDerivativeTable>{body}</nonDerivativeTable></ownershipDocument>"
        )
    }

    #[test]
    fn only_open_market_purchases_and_sales_are_kept() {
        // Regression: this used to return 0 trades for every real filing
        // (transactionCode was looked up as a nested <value>), and before that
        // it returned tax-withholding and grants as if they were sales/buys.
        let xml = form4(
            "Chief Financial Officer",
            &[
                ("2024-05-01", "P", "100", "A"),  // open-market buy
                ("2024-05-02", "S", "250", "D"),  // open-market sale
                ("2024-05-03", "F", "999", "D"),  // tax withholding: NOT a view
                ("2024-05-04", "A", "500", "A"),  // grant: NOT a view
                ("2024-05-05", "M", "300", "A"),  // option exercise: NOT a view
                ("2024-05-06", "G", "50", "D"),   // gift: NOT a view
            ],
        );
        let trades = parse_form4_xml("XYZ", d("2024-05-07"), &xml);
        assert_eq!(trades.len(), 2, "{trades:?}");
        assert_eq!(trades[0].transaction_type, "A");
        assert_eq!(trades[0].shares, 100.0);
        assert_eq!(trades[1].transaction_type, "D");
        assert_eq!(trades[1].trade_date, d("2024-05-02"));
        assert!(trades.iter().all(|t| t.filing_date == d("2024-05-07")));
        assert!(trades.iter().all(|t| t.insider_role == "CFO" && t.insider_name == "DOE JANE"));
    }

    #[test]
    fn a_filing_with_only_compensation_events_yields_nothing() {
        let xml = form4("Director", &[("2024-05-03", "F", "999", "D"), ("2024-05-04", "A", "500", "A")]);
        assert!(parse_form4_xml("XYZ", d("2024-05-07"), &xml).is_empty());
    }

    #[test]
    fn a_vice_president_is_not_treated_as_the_president() {
        let xml = form4("Vice President, Sales", &[("2024-05-01", "P", "10", "A")]);
        assert_eq!(parse_form4_xml("X", d("2024-05-02"), &xml)[0].insider_role, "Officer");
        let xml = form4("President", &[("2024-05-01", "P", "10", "A")]);
        assert_eq!(parse_form4_xml("X", d("2024-05-02"), &xml)[0].insider_role, "President");
    }

    #[test]
    fn the_raw_xml_filename_drops_the_xsl_rendering_directory() {
        // EDGAR's index points at the HTML-rendered view; the raw XML is one level up.
        assert_eq!(raw_form4_filename("xslF345X06/wk-form4_1789766132.xml"), "wk-form4_1789766132.xml");
        assert_eq!(raw_form4_filename("form4.xml"), "form4.xml");
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A minimal local HTTP/1.1 server: serves `responses` in order (one per
    /// connection), then keeps repeating the last one. Used to test retry
    /// behaviour against a REAL flaky endpoint rather than mocked results.
    async fn flaky_server(responses: Vec<&'static [u8]>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut i = 0usize;
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let body = responses[i.min(responses.len() - 1)];
                i += 1;
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await; // drain the request
                let _ = sock.write_all(body).await;
                let _ = sock.shutdown().await;
            }
        });
        format!("http://{addr}/x")
    }

    const OK_BODY: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\nConnection: close\r\n\r\n{\"cik\":12345}";
    const GARBAGE_BODY: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    const SERVER_ERROR: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

    #[derive(serde::Deserialize, Debug)]
    struct Resp {
        cik: u64,
    }

    #[tokio::test]
    async fn recovers_after_a_transient_failure() {
        // Exactly what was observed live: the first attempt returns an empty
        // body (an unparsable "expected value at line 1 column 1" in
        // production), the second succeeds.
        let url = flaky_server(vec![GARBAGE_BODY, OK_BODY]).await;
        let client = Client::new();
        let r: Resp = fetch_json_with_retries(&client, &url, 3).await.unwrap();
        assert_eq!(r.cik, 12345);
    }

    #[tokio::test]
    async fn recovers_from_a_5xx_status_not_just_a_bad_body() {
        let url = flaky_server(vec![SERVER_ERROR, SERVER_ERROR, OK_BODY]).await;
        let client = Client::new();
        let r: Resp = fetch_json_with_retries(&client, &url, 3).await.unwrap();
        assert_eq!(r.cik, 12345);
    }

    #[tokio::test]
    async fn gives_up_after_the_configured_attempt_count() {
        let url = flaky_server(vec![GARBAGE_BODY]).await;
        let client = Client::new();
        let err = fetch_json_with_retries::<Resp>(&client, &url, 2).await.unwrap_err();
        assert!(format!("{err:#}").contains("body was not valid JSON"), "{err:#}");
    }

    #[tokio::test]
    async fn a_single_immediate_success_needs_no_retry() {
        let url = flaky_server(vec![OK_BODY]).await;
        let client = Client::new();
        let r: Resp = fetch_json_with_retries(&client, &url, 3).await.unwrap();
        assert_eq!(r.cik, 12345);
    }
}
