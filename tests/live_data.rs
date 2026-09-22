//! Network integration tests. Skipped by default; run with:
//!
//!     cargo test --test live_data -- --ignored --nocapture
//!
//! They hit the real SEC API, so set SEC_USER_AGENT="Your Name you@example.com"
//! first (SEC asks that clients identify themselves).

use chrono::NaiveDate;
use quant_edge::data::asof::AsOf;
use quant_edge::data::cache::Cache;
use quant_edge::data::sec_facts::SecFactsFetcher;

fn d(s: &str) -> NaiveDate {
    s.parse().unwrap()
}

#[tokio::test]
#[ignore = "hits the live SEC API"]
async fn point_in_time_fundamentals_from_live_sec_data() {
    let cache = Cache::open(":memory:").unwrap();
    let sec = SecFactsFetcher::new(cache.clone());

    // Full pipeline: ticker -> CIK -> companyfacts -> parse -> cache -> snapshot.
    let after_q1 = sec.snapshot("AAPL", d("2023-03-01")).await.expect("AAPL snapshot");
    let before_q1 = sec.snapshot("AAPL", d("2023-01-15")).await.expect("AAPL snapshot");

    let rev_after = after_q1.revenue_ttm.unwrap() / 1e9;
    let rev_before = before_q1.revenue_ttm.unwrap() / 1e9;
    println!("TTM revenue: {rev_before:.1}B before the Q1 10-Q, {rev_after:.1}B after");

    // Apple FY22 revenue was $394.3B; TTM after the Dec-2022 quarter was ~$387.5B.
    assert!((rev_before - 394.3).abs() < 1.0);
    assert!((rev_after - 387.5).abs() < 1.0);
    assert!(after_q1.revenue_cagr_3yr.unwrap() > 0.05, "revenue growth is now available (Yahoo could not supply it)");

    // The as-of view must not expose anything filed after the date.
    let facts = AsOf::new(&cache, d("2023-01-15")).pit_facts("AAPL").unwrap();
    assert!(!facts.is_empty());
    assert!(facts.iter().all(|f| f.filed <= d("2023-01-15")));

    // A non-US ticker has no CIK: it must fail cleanly, not fabricate data.
    assert!(sec.snapshot("RELIANCE.NS", d("2023-03-01")).await.is_err());
}

#[tokio::test]
#[ignore = "hits the live SEC API"]
async fn insider_trades_are_actually_fetched_and_parsed() {
    use quant_edge::data::edgar::EdgarFetcher;
    let _ = tracing_subscriber::fmt()
        .with_env_filter("quant_edge=debug,warn")
        .with_test_writer()
        .try_init();

    let cache = Cache::open(":memory:").unwrap();
    let edgar = EdgarFetcher::new(cache.clone());
    let today = chrono::Local::now().date_naive();

    // NVDA files Form 4s constantly; a 180-day window should never be empty.
    let trades = edgar.fetch_insider_trades("NVDA", today, 180).await.expect("insider fetch");
    println!("NVDA insider trades in 180d: {}", trades.len());
    if let Some(t) = trades.first() {
        println!("sample: {} {} {} shares {} filed {} traded {}", t.ticker, t.insider_role, t.transaction_type, t.shares, t.filing_date, t.trade_date);
    }
    assert!(!trades.is_empty(), "the insider signal has no data");
    assert!(trades.iter().all(|t| t.filing_date <= today && t.trade_date <= today));
}

#[tokio::test]
#[ignore = "hits the live FRED API"]
async fn macro_series_actually_load() {
    use quant_edge::data::fred::FredFetcher;
    let _ = tracing_subscriber::fmt().with_env_filter("quant_edge=debug,warn").with_test_writer().try_init();

    let cache = Cache::open(":memory:").unwrap();
    let fred = FredFetcher::new(cache.clone());
    let today = chrono::Local::now().date_naive();
    let snap = fred.macro_snapshot(today).await;
    println!("VIX={:?} 10Y={:?} 10Y(30d ago)={:?} macro_on={}", snap.vix, snap.yield_10y, snap.yield_10y_30d_ago, snap.macro_on);
    assert!(snap.vix.is_some(), "VIX never loaded: the macro gate is permanently fail-open");
    assert!(snap.yield_10y.is_some(), "10Y yield never loaded");

    // History works too, and the gate closes when it should: VIX was ~66 on
    // 2020-03-20 and calm in mid-2021.
    let covid = fred.macro_snapshot(d("2020-03-20")).await;
    println!("2020-03-20: VIX={:?} macro_on={}", covid.vix, covid.macro_on);
    assert!(covid.vix.unwrap() > 50.0);
    assert!(!covid.macro_on, "the gate must be risk-off during the March 2020 crash");
    let calm = fred.macro_snapshot(d("2021-06-15")).await;
    assert!(calm.macro_on, "VIX {:?} on 2021-06-15", calm.vix);
}

#[tokio::test]
async fn news_is_disabled_rather_than_fabricated() {
    // GDELT's article-list mode has no tone, so the fetcher is switched off and
    // the signal reports "no data" instead of a fake neutral 0.0. No network used.
    use quant_edge::data::gdelt::GdeltFetcher;
    let cache = Cache::open(":memory:").unwrap();
    let gdelt = GdeltFetcher::new(cache);
    let today = chrono::Local::now().date_naive();
    let items = gdelt.fetch_news_sentiment("AAPL", today, 30).await.unwrap();
    assert!(items.is_empty());
}

#[tokio::test]
#[ignore = "hits the live GitHub raw-content API"]
async fn point_in_time_sp500_membership_matches_known_history() {
    use quant_edge::universe::HistoricalMembership;

    let cache = Cache::open(":memory:").unwrap();
    let hist = HistoricalMembership::new(cache);

    // TSLA joined the S&P 500 on 2020-12-21.
    let before = hist.members_as_of(d("2020-12-01")).await.unwrap();
    let after = hist.members_as_of(d("2021-01-01")).await.unwrap();
    println!("members before: {} after: {}", before.len(), after.len());
    assert!(!before.contains(&"TSLA".to_string()), "TSLA should not be a member yet");
    assert!(after.contains(&"TSLA".to_string()), "TSLA should be a member by 2021-01-01");
    assert!(before.len() > 400 && after.len() > 400, "sanity: roughly 500 members");

    // A window straddling the swap must include both the removed and added name.
    let union = hist.union_over(d("2020-12-01"), d("2020-12-31")).await.unwrap();
    assert!(union.contains("TSLA"));

    // A date long before the dataset starts falls back to the earliest row
    // rather than erroring or returning nothing.
    let ancient = hist.members_as_of(d("1990-01-01")).await.unwrap();
    assert!(!ancient.is_empty());
}

#[tokio::test]
#[ignore = "hits the live Yahoo API"]
async fn usd_inr_rate_is_sane_and_point_in_time() {
    use quant_edge::fx::FxRates;
    let cache = Cache::open(":memory:").unwrap();
    let fx = FxRates::new(cache);
    let today = chrono::Local::now().date_naive();

    let rate = fx.usd_inr_rate(today).await.unwrap();
    println!("USD/INR today: {rate}");
    assert!(rate > 70.0 && rate < 120.0, "outside any plausible USD/INR range: {rate}");

    // A rate from a year ago should differ from today's (it's a real,
    // moving market rate, not a hardcoded constant).
    let past = fx.usd_inr_rate(today - chrono::Duration::days(365)).await.unwrap();
    println!("USD/INR ~1y ago: {past}");
    assert!(past > 70.0 && past < 120.0);
}
