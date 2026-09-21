//! Point-in-time fundamentals from SEC XBRL "company facts".
//!
//! Yahoo's fundamentals endpoint only ever returns the *latest* numbers, so
//! using it for a historical date leaks the future (and cannot supply revenue
//! growth at all). SEC facts fix that: every value carries the date it was
//! **filed**, so "what did the market know on date D" is simply "facts with
//! filed <= D", and when a company later restates a period we correctly keep
//! the originally-reported figure until the restatement is filed.
//!
//! Only 10-K / 10-Q family filings are used (not 8-K earnings releases): that
//! is slightly later than the true first-public moment, which errs on the safe
//! side and can never leak.
//!
//! US-listed issuers only. Tickers with no CIK (e.g. `.NS`) get no PIT
//! fundamentals, and the signal layer reports that rather than faking it.

use anyhow::{anyhow, Context, Result};
use chrono::{Duration, NaiveDate};
use serde_json::Value;
use std::collections::BTreeMap;
use tracing::{debug, info};

use super::cache::Cache;
use super::edgar::EdgarFetcher;
use super::source::FundamentalSnapshot;

// ── Types ─────────────────────────────────────────────────────────────────────

/// One reported value, tagged with when it became public.
#[derive(Debug, Clone, PartialEq)]
pub struct PitFact {
    /// Logical concept (`revenue`, `net_income`, ...), not the raw XBRL tag —
    /// companies switch tags over time and we merge them.
    pub concept: String,
    /// Period start. `None` for instant (balance-sheet) facts.
    pub start: Option<NaiveDate>,
    pub end: NaiveDate,
    pub value: f64,
    /// Date the filing containing this value was made public.
    pub filed: NaiveDate,
    pub form: String,
}

impl PitFact {
    fn duration_days(&self) -> Option<i64> {
        self.start.map(|s| (self.end - s).num_days())
    }
}

/// (taxonomy, XBRL tag, logical concept, unit). Earlier rows win when two tags
/// report the same period.
const MAPPING: &[(&str, &str, &str, &str)] = &[
    ("us-gaap", "Revenues", "revenue", "USD"),
    ("us-gaap", "RevenueFromContractWithCustomerExcludingAssessedTax", "revenue", "USD"),
    ("us-gaap", "RevenueFromContractWithCustomerIncludingAssessedTax", "revenue", "USD"),
    ("us-gaap", "SalesRevenueNet", "revenue", "USD"),
    ("us-gaap", "SalesRevenueGoodsNet", "revenue", "USD"),
    ("us-gaap", "NetIncomeLoss", "net_income", "USD"),
    ("us-gaap", "GrossProfit", "gross_profit", "USD"),
    ("us-gaap", "NetCashProvidedByUsedInOperatingActivities", "operating_cash_flow", "USD"),
    ("us-gaap", "Assets", "assets", "USD"),
    ("us-gaap", "StockholdersEquity", "equity", "USD"),
    ("us-gaap", "StockholdersEquityIncludingPortionAttributableToNoncontrollingInterest", "equity", "USD"),
    ("us-gaap", "LongTermDebtNoncurrent", "long_term_debt", "USD"),
    ("us-gaap", "LongTermDebt", "long_term_debt", "USD"),
    ("us-gaap", "LongTermDebtCurrent", "short_term_debt", "USD"),
    ("us-gaap", "DebtCurrent", "short_term_debt", "USD"),
    ("dei", "EntityCommonStockSharesOutstanding", "shares_outstanding", "shares"),
];

const ACCEPTED_FORMS: &[&str] = &["10-K", "10-K/A", "10-Q", "10-Q/A"];

/// Reported data older than this at `as_of` is considered stale.
const MAX_STALENESS_DAYS: i64 = 550;

// ── Parsing ───────────────────────────────────────────────────────────────────

/// Extract facts from a `companyfacts` JSON document.
pub fn parse_companyfacts(json: &Value) -> Vec<PitFact> {
    let mut out: Vec<PitFact> = Vec::new();
    let mut seen: std::collections::HashSet<(String, Option<NaiveDate>, NaiveDate, NaiveDate)> =
        std::collections::HashSet::new();

    for (taxonomy, tag, concept, unit) in MAPPING {
        let Some(entries) = json
            .pointer(&format!("/facts/{taxonomy}/{tag}/units/{unit}"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for e in entries {
            let form = e.get("form").and_then(Value::as_str).unwrap_or("");
            if !ACCEPTED_FORMS.contains(&form) {
                continue;
            }
            let parse = |k: &str| {
                e.get(k)
                    .and_then(Value::as_str)
                    .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
            };
            let (Some(end), Some(filed), Some(value)) =
                (parse("end"), parse("filed"), e.get("val").and_then(Value::as_f64))
            else {
                continue;
            };
            let start = parse("start");
            // First tag to report a (period, filing) wins.
            if !seen.insert((concept.to_string(), start, end, filed)) {
                continue;
            }
            out.push(PitFact {
                concept: concept.to_string(),
                start,
                end,
                value,
                filed,
                form: form.to_string(),
            });
        }
    }
    out
}

// ── Snapshot builder (pure) ───────────────────────────────────────────────────

/// For each (start, end) period keep the value from the latest filing that was
/// public by `as_of`. This is what the market believed at `as_of`, restatements
/// included only once they were actually filed.
fn latest_known<'a>(
    facts: &'a [PitFact],
    concept: &str,
    as_of: NaiveDate,
) -> BTreeMap<(Option<NaiveDate>, NaiveDate), &'a PitFact> {
    let mut map: BTreeMap<(Option<NaiveDate>, NaiveDate), &PitFact> = BTreeMap::new();
    for f in facts.iter().filter(|f| f.concept == concept && f.filed <= as_of) {
        map.entry((f.start, f.end))
            .and_modify(|cur| {
                if f.filed > cur.filed {
                    *cur = f;
                }
            })
            .or_insert(f);
    }
    map
}

fn is_annual(f: &PitFact) -> bool {
    f.duration_days().map_or(false, |d| (350..=380).contains(&d))
}

/// Trailing-twelve-months of a flow item as known at `as_of`:
/// `latest FY + current YTD − same YTD a year earlier`.
fn ttm(facts: &[PitFact], concept: &str, as_of: NaiveDate) -> Option<f64> {
    let known = latest_known(facts, concept, as_of);
    let annual = known.values().filter(|f| is_annual(f)).max_by_key(|f| f.end)?;
    if (as_of - annual.end).num_days() > MAX_STALENESS_DAYS {
        return None;
    }

    // Cumulative year-to-date period that starts right after that fiscal year.
    let ytd = known
        .values()
        .filter(|f| {
            f.start.map_or(false, |s| {
                let gap = (s - annual.end).num_days();
                (1..=7).contains(&gap)
            }) && f.duration_days().map_or(false, |d| (80..=290).contains(&d))
                && f.end > annual.end
        })
        .max_by_key(|f| f.end);

    let Some(ytd) = ytd else {
        return Some(annual.value);
    };

    // Same cumulative period one year earlier (52/53-week years drift by days).
    let (ys, ye) = (ytd.start?, ytd.end);
    let prior = known.values().find(|f| {
        f.start.map_or(false, |s| ((ys - s).num_days() - 365).abs() <= 10)
            && ((ye - f.end).num_days() - 365).abs() <= 10
    });

    match prior {
        Some(p) => Some(annual.value + ytd.value - p.value),
        None => Some(annual.value),
    }
}

/// Latest balance-sheet value known at `as_of`.
fn latest_instant(facts: &[PitFact], concept: &str, as_of: NaiveDate) -> Option<f64> {
    let known = latest_known(facts, concept, as_of);
    let f = known.values().filter(|f| f.start.is_none()).max_by_key(|f| (f.end, f.filed))?;
    ((as_of - f.end).num_days() <= MAX_STALENESS_DAYS).then_some(f.value)
}

/// 3-year revenue CAGR from fiscal-year totals known at `as_of` (fraction).
fn revenue_cagr_3yr(facts: &[PitFact], as_of: NaiveDate) -> Option<f64> {
    let known = latest_known(facts, "revenue", as_of);
    let annuals: Vec<&&PitFact> = known.values().filter(|f| is_annual(f)).collect();
    let latest = annuals.iter().max_by_key(|f| f.end)?;
    if (as_of - latest.end).num_days() > MAX_STALENESS_DAYS {
        return None;
    }
    let target = latest.end - Duration::days((3.0 * 365.25) as i64);
    let base = annuals
        .iter()
        .filter(|f| (f.end - target).num_days().abs() <= 30)
        .min_by_key(|f| (f.end - target).num_days().abs())?;
    if base.value <= 0.0 || latest.value <= 0.0 {
        return None;
    }
    Some((latest.value / base.value).powf(1.0 / 3.0) - 1.0)
}

/// Build the fundamentals a market participant could have computed at
/// `as_of`. `price` is the raw (unadjusted) close on/before `as_of`, used only
/// for price-to-book. Returns `None` when nothing meaningful is available.
pub fn build_snapshot(
    ticker: &str,
    facts: &[PitFact],
    as_of: NaiveDate,
    price: Option<f64>,
) -> Option<FundamentalSnapshot> {
    let revenue_ttm = ttm(facts, "revenue", as_of).filter(|v| *v > 0.0);
    let net_income = ttm(facts, "net_income", as_of);
    let gross_profit = ttm(facts, "gross_profit", as_of);
    let operating_cashflow = ttm(facts, "operating_cash_flow", as_of);
    let assets = latest_instant(facts, "assets", as_of).filter(|v| *v > 0.0);
    let equity = latest_instant(facts, "equity", as_of);
    let shares = latest_instant(facts, "shares_outstanding", as_of).filter(|v| *v > 0.0);

    let net_margin_pct = match (net_income, revenue_ttm) {
        (Some(n), Some(r)) => Some(n / r * 100.0),
        _ => None,
    };
    let gross_profit_margin = match (gross_profit, revenue_ttm) {
        (Some(g), Some(r)) => Some(g / r * 100.0),
        _ => None,
    };
    let return_on_assets = match (net_income, assets) {
        (Some(n), Some(a)) => Some(n / a * 100.0),
        _ => None,
    };

    // Approximate: latest long-term debt + latest current debt (each may be
    // from a slightly different balance-sheet date). Negative/zero equity
    // makes D/E meaningless, so it is left unknown rather than guessed.
    let debt = {
        let lt = latest_instant(facts, "long_term_debt", as_of);
        let st = latest_instant(facts, "short_term_debt", as_of);
        match (lt, st) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        }
    };
    let debt_to_equity = match (debt, equity) {
        (Some(d), Some(e)) if e > 0.0 => Some(d / e * 100.0),
        _ => None,
    };

    let price_to_book = match (price, shares, equity) {
        (Some(p), Some(sh), Some(e)) if p > 0.0 && e > 0.0 => Some(p * sh / e),
        _ => None,
    };

    let snap = FundamentalSnapshot {
        ticker: ticker.to_string(),
        date: as_of,
        revenue_ttm,
        revenue_cagr_3yr: revenue_cagr_3yr(facts, as_of),
        net_margin_pct,
        debt_to_equity,
        price_to_book,
        price_return_12m_1m: None, // derived from prices by the caller
        market_share_proxy: None,
        operating_cashflow,
        return_on_assets,
        gross_profit_margin,
    };

    let has_any = snap.revenue_ttm.is_some()
        || snap.net_margin_pct.is_some()
        || snap.return_on_assets.is_some()
        || snap.debt_to_equity.is_some()
        || snap.price_to_book.is_some();
    has_any.then_some(snap)
}

// ── Fetcher ───────────────────────────────────────────────────────────────────

/// Downloads SEC company facts into the cache and serves point-in-time
/// snapshots from it.
pub struct SecFactsFetcher {
    edgar: EdgarFetcher,
    cache: Cache,
}

impl SecFactsFetcher {
    pub fn new(cache: Cache) -> Self {
        Self { edgar: EdgarFetcher::new(cache.clone()), cache }
    }

    /// Make sure facts for `ticker` are in the cache (re-downloads at most
    /// weekly). Facts are immutable once filed, so this is safe for any date.
    pub async fn ensure_facts(&self, ticker: &str) -> Result<()> {
        if self.cache.has_pit_facts(ticker, 7) {
            return Ok(());
        }
        let json = self
            .edgar
            .company_facts(ticker)
            .await
            .with_context(|| format!("SEC company facts unavailable for {ticker}"))?;
        let facts = parse_companyfacts(&json);
        if facts.is_empty() {
            return Err(anyhow!("SEC returned no usable 10-K/10-Q facts for {ticker}"));
        }
        let n = self.cache.insert_pit_facts(ticker, &facts)?;
        info!(ticker = %ticker, new_rows = n, total = facts.len(), "SEC facts cached");
        Ok(())
    }

    /// Point-in-time snapshot for `ticker` as of `as_of`.
    pub async fn snapshot(&self, ticker: &str, as_of: NaiveDate) -> Result<FundamentalSnapshot> {
        self.ensure_facts(ticker).await?;
        let facts = self.cache.get_pit_facts(ticker, as_of)?;

        let price = self
            .cache
            .get_price_bars(ticker, as_of - Duration::days(10), as_of)
            .ok()
            .and_then(|bars| bars.iter().filter(|b| b.date <= as_of).last().map(|b| b.close));

        debug!(ticker = %ticker, facts = facts.len(), "building PIT snapshot");
        build_snapshot(ticker, &facts, as_of, price)
            .ok_or_else(|| anyhow!("no point-in-time fundamentals for {ticker} as of {as_of}"))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn flow(concept: &str, start: &str, end: &str, value: f64, filed: &str) -> PitFact {
        PitFact {
            concept: concept.into(),
            start: Some(d(start)),
            end: d(end),
            value,
            filed: d(filed),
            form: "10-Q".into(),
        }
    }

    fn instant(concept: &str, end: &str, value: f64, filed: &str) -> PitFact {
        PitFact { concept: concept.into(), start: None, end: d(end), value, filed: d(filed), form: "10-Q".into() }
    }

    /// A company with a Sep fiscal year: FY21 → FY22 → Q1 FY23.
    fn company() -> Vec<PitFact> {
        vec![
            flow("revenue", "2020-10-01", "2021-09-30", 800.0, "2021-11-01"),
            flow("revenue", "2021-10-01", "2022-09-30", 1_000.0, "2022-11-01"),
            flow("revenue", "2021-10-01", "2021-12-31", 250.0, "2022-02-01"), // Q1 FY22
            flow("revenue", "2022-10-01", "2022-12-31", 300.0, "2023-02-01"), // Q1 FY23
            flow("net_income", "2021-10-01", "2022-09-30", 100.0, "2022-11-01"),
            instant("assets", "2022-09-30", 2_000.0, "2022-11-01"),
            instant("equity", "2022-09-30", 500.0, "2022-11-01"),
            instant("long_term_debt", "2022-09-30", 300.0, "2022-11-01"),
            instant("shares_outstanding", "2022-10-20", 10.0, "2022-11-01"),
        ]
    }

    #[test]
    fn ttm_adds_current_ytd_and_subtracts_prior_year_ytd() {
        // FY22 (1000) + Q1 FY23 (300) − Q1 FY22 (250) = 1050
        let v = ttm(&company(), "revenue", d("2023-03-01")).unwrap();
        assert!((v - 1_050.0).abs() < 1e-9, "{v}");
    }

    #[test]
    fn point_in_time_ignores_filings_not_yet_public() {
        // Q1 FY23 is filed 2023-02-01; on 2022-12-15 the market only knew FY22.
        let before = ttm(&company(), "revenue", d("2022-12-15")).unwrap();
        assert!((before - 1_000.0).abs() < 1e-9, "{before}");
        // Before the FY22 10-K (2022-11-01) only FY21 was known.
        let earlier = ttm(&company(), "revenue", d("2022-10-15")).unwrap();
        assert!((earlier - 800.0).abs() < 1e-9, "{earlier}");
    }

    #[test]
    fn restatements_apply_only_once_filed() {
        let mut f = company();
        // FY22 revenue restated to 1100 in a later filing.
        f.push(flow("revenue", "2021-10-01", "2022-09-30", 1_100.0, "2023-11-01"));
        // Shortly after the restatement was filed, FY24 not yet reported.
        let before = ttm(&f, "revenue", d("2022-12-15")).unwrap();
        assert!((before - 1_000.0).abs() < 1e-9, "old value until the restatement is filed");
        let known_annual = latest_known(&f, "revenue", d("2023-12-01"));
        let fy22 = known_annual.get(&(Some(d("2021-10-01")), d("2022-09-30"))).unwrap();
        assert_eq!(fy22.value, 1_100.0);
    }

    #[test]
    fn stale_data_is_dropped() {
        assert!(ttm(&company(), "revenue", d("2026-01-01")).is_none());
        assert!(latest_instant(&company(), "assets", d("2026-01-01")).is_none());
    }

    #[test]
    fn snapshot_derives_ratios() {
        let s = build_snapshot("X", &company(), d("2023-03-01"), Some(60.0)).unwrap();
        assert!((s.revenue_ttm.unwrap() - 1_050.0).abs() < 1e-9);
        assert!((s.debt_to_equity.unwrap() - 60.0).abs() < 1e-9); // 300/500 as percent
        assert!((s.return_on_assets.unwrap() - 5.0).abs() < 1e-9); // 100/2000
        // P/B = 60 * 10 shares / 500 equity = 1.2
        assert!((s.price_to_book.unwrap() - 1.2).abs() < 1e-9);
    }

    #[test]
    fn snapshot_is_none_before_any_filing() {
        assert!(build_snapshot("X", &company(), d("2021-01-01"), None).is_none());
    }

    #[test]
    fn negative_equity_leaves_leverage_unknown() {
        let mut f = company();
        f.retain(|x| x.concept != "equity");
        f.push(instant("equity", "2022-09-30", -50.0, "2022-11-01"));
        let s = build_snapshot("X", &f, d("2023-03-01"), Some(60.0)).unwrap();
        assert!(s.debt_to_equity.is_none());
        assert!(s.price_to_book.is_none());
    }

    #[test]
    fn revenue_cagr_uses_fiscal_years_known_at_the_date() {
        let mut f = company();
        f.push(flow("revenue", "2018-10-01", "2019-09-30", 500.0, "2019-11-01"));
        f.push(flow("revenue", "2019-10-01", "2020-09-30", 600.0, "2020-11-01"));
        // Latest FY22 = 1000; three years earlier FY19 = 500 → 2^(1/3) − 1
        let cagr = revenue_cagr_3yr(&f, d("2023-03-01")).unwrap();
        assert!((cagr - (2.0f64.powf(1.0 / 3.0) - 1.0)).abs() < 1e-9, "{cagr}");
        // Before the FY22 10-K was filed the latest known year is FY21, whose
        // three-year base (FY18) is not in the data, so growth is unknown, not guessed.
        assert!(revenue_cagr_3yr(&f, d("2022-10-15")).is_none());
        // ...and with the FY18 base present it is computed from FY21, not FY22.
        f.push(flow("revenue", "2017-10-01", "2018-09-30", 400.0, "2018-11-01"));
        let earlier = revenue_cagr_3yr(&f, d("2022-10-15")).unwrap();
        assert!((earlier - ((800.0f64 / 400.0).powf(1.0 / 3.0) - 1.0)).abs() < 1e-9, "{earlier}");
    }

    #[test]
    fn parse_keeps_only_periodic_filings_and_merges_tags() {
        let doc = json!({
            "facts": {
                "us-gaap": {
                    "Revenues": { "units": { "USD": [
                        { "start": "2017-10-01", "end": "2018-09-29", "val": 200.0, "form": "10-K", "filed": "2018-11-05" }
                    ]}},
                    "RevenueFromContractWithCustomerExcludingAssessedTax": { "units": { "USD": [
                        { "start": "2018-09-30", "end": "2019-09-28", "val": 260.0, "form": "10-K", "filed": "2019-10-31" },
                        { "start": "2019-09-29", "end": "2019-12-28", "val": 90.0, "form": "8-K", "filed": "2020-01-28" }
                    ]}},
                    "Assets": { "units": { "USD": [
                        { "end": "2019-09-28", "val": 900.0, "form": "10-K", "filed": "2019-10-31" }
                    ]}}
                },
                "dei": { "EntityCommonStockSharesOutstanding": { "units": { "shares": [
                    { "end": "2019-10-18", "val": 4.5, "form": "10-K", "filed": "2019-10-31" }
                ]}}}
            }
        });
        let facts = parse_companyfacts(&doc);
        assert_eq!(facts.iter().filter(|f| f.concept == "revenue").count(), 2, "both tags merge; the 8-K is dropped");
        assert!(facts.iter().all(|f| f.form != "8-K"));
        let assets = facts.iter().find(|f| f.concept == "assets").unwrap();
        assert!(assets.start.is_none());
        assert!(facts.iter().any(|f| f.concept == "shares_outstanding"));
    }

    #[test]
    fn duplicate_tags_for_the_same_period_keep_the_first() {
        let doc = json!({
            "facts": { "us-gaap": {
                "StockholdersEquity": { "units": { "USD": [
                    { "end": "2020-12-31", "val": 100.0, "form": "10-K", "filed": "2021-02-01" } ]}},
                "StockholdersEquityIncludingPortionAttributableToNoncontrollingInterest": { "units": { "USD": [
                    { "end": "2020-12-31", "val": 130.0, "form": "10-K", "filed": "2021-02-01" } ]}}
            }}
        });
        let facts = parse_companyfacts(&doc);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].value, 100.0, "parent-only equity is listed first and wins");
    }

    /// Opt-in check against a real download:
    ///   curl -A "you you@example.com" https://data.sec.gov/api/xbrl/companyfacts/CIK0000320193.json -o aapl.json
    ///   SEC_FACTS_FIXTURE=aapl.json cargo test real_apple -- --ignored
    #[test]
    #[ignore = "needs SEC_FACTS_FIXTURE pointing at a real companyfacts JSON for Apple"]
    fn real_apple_facts_match_reported_numbers() {
        let path = std::env::var("SEC_FACTS_FIXTURE").expect("SEC_FACTS_FIXTURE");
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let facts = parse_companyfacts(&doc);
        assert!(facts.len() > 500, "parsed {}", facts.len());

        // After the Q1 FY23 10-Q (filed 2023-02-03): FY22 + Q1'23 - Q1'22
        // = 394.328 + 117.154 - 123.945 = 387.537 (USD billions).
        let snap = build_snapshot("AAPL", &facts.iter().filter(|f| f.filed <= d("2023-03-01")).cloned().collect::<Vec<_>>(), d("2023-03-01"), Some(150.0)).unwrap();
        let rev_b = snap.revenue_ttm.unwrap() / 1e9;
        assert!((rev_b - 387.5).abs() < 1.0, "TTM revenue {rev_b}B");
        assert!(snap.net_margin_pct.unwrap() > 20.0 && snap.net_margin_pct.unwrap() < 30.0);
        assert!(snap.revenue_cagr_3yr.unwrap() > 0.05, "3y CAGR {:?}", snap.revenue_cagr_3yr);
        assert!(snap.price_to_book.unwrap() > 10.0, "AAPL trades at a high P/B on tiny equity");

        // Point-in-time: a month before that 10-Q the market still saw FY22 only.
        let before = ttm(&facts, "revenue", d("2023-01-15")).unwrap() / 1e9;
        assert!((before - 394.3).abs() < 1.0, "TTM before the 10-Q {before}B");
    }
}
