use anyhow::{Context, Result};
use chrono::{Duration, NaiveDate};
use tracing::warn;

use crate::data::{cache::Cache, yahoo::YahooFinance};
use crate::gics::GicsTaxonomy;
use crate::paper_trading::engine::PaperTradingEngine;
use crate::signals::{PickingEngine, SignalScore};
use crate::universe::{AutoUniverseBuilder, UniverseBuilder, UniverseConfig, Market, CapFilter};

use super::{holding_period_days, min_score_threshold, score_based_picks};

/// Run the morning report. Returns the formatted output string (also saved to logs/).
pub async fn run_morning(cache: Cache, taxonomy: &GicsTaxonomy, today: NaiveDate) -> Result<String> {
    let threshold = min_score_threshold();
    let hold_days = holding_period_days();

    // ── 1. Auto universe ──────────────────────────────────────────────────────
    let auto_builder = AutoUniverseBuilder::new(cache.clone());
    let tickers = auto_builder
        .get_all_tickers()
        .await
        .context("Auto universe fetch failed")?;

    let source = YahooFinance::new(cache.clone());
    let ub = UniverseBuilder::new(&source, taxonomy);
    let config = UniverseConfig {
        market:                  Market::Both,
        cap_filter:              CapFilter::Mixed,
        n_industries:            100,
        exclude_industry_codes:  vec![],
    };

    let mut universe = ub
        .build_from_tickers(tickers, config)
        .await
        .context("Universe build failed")?;

    ub.enrich_gics(&mut universe)
        .await
        .context("GICS enrichment failed")?;

    universe.trim_to_n_industries(100, &[]);

    // ── 2. Run picking engine ─────────────────────────────────────────────────
    let engine = PickingEngine::new(cache.clone());
    let scores = engine
        .rank_universe(&universe, today)
        .await
        .context("Picking engine failed")?;

    // ── 3. Score-based top-N selection ────────────────────────────────────────
    let picks = score_based_picks(&scores, threshold);

    let nse_picks: Vec<SignalScore> = picks
        .iter()
        .filter(|s| s.ticker.ends_with(".NS"))
        .cloned()
        .collect();
    let us_picks: Vec<SignalScore> = picks
        .iter()
        .filter(|s| !s.ticker.ends_with(".NS"))
        .cloned()
        .collect();

    // ── 4. Build report ───────────────────────────────────────────────────────
    let mut out = String::new();

    out.push_str(&format!(
        "\n\x1b[1m\x1b[97m══════════════════════════════════════════════════════════════\x1b[0m\n"
    ));
    out.push_str(&format!(
        "\x1b[1m\x1b[97m  MORNING REPORT — {}\x1b[0m\n",
        today
    ));
    out.push_str(&format!(
        "\x1b[1m\x1b[97m══════════════════════════════════════════════════════════════\x1b[0m\n"
    ));
    out.push_str(&format!(
        "  Score threshold: {:.0}   Holding period: {} days   Universe: {} tickers scored\n\n",
        threshold, hold_days, scores.len()
    ));

    // NSE section
    out.push_str(&format_picks_section("NSE PICKS", &nse_picks, &cache, today));

    // US section
    out.push_str(&format_picks_section("US PICKS", &us_picks, &cache, today));

    // Market context
    out.push_str(&format_market_context(&cache, today).await);

    // Portfolio status
    out.push_str(&format_portfolio_section(&cache, today, hold_days));

    // Separator
    out.push_str(&format!(
        "\n\x1b[1m\x1b[97m══════════════════════════════════════════════════════════════\x1b[0m\n"
    ));

    // ── 5. Strip ANSI for file save, keep colour for terminal ─────────────────
    let plain = strip_ansi(&out);
    std::fs::create_dir_all("logs").ok();
    let log_path = format!("logs/morning_{}.txt", today.format("%Y%m%d"));
    std::fs::write(&log_path, &plain)
        .with_context(|| format!("Failed to write {}", log_path))?;

    Ok(out)
}

// ── Formatting helpers ─────────────────────────────────────────────────────────

fn format_picks_section(title: &str, picks: &[SignalScore], cache: &Cache, today: NaiveDate) -> String {
    let mut s = String::new();
    s.push_str(&format!("  \x1b[1m{}\x1b[0m  ({} picks)\n", title, picks.len()));
    s.push_str("  ─────────────────────────────────────────────────────────────────────────────────────────────\n");

    if picks.is_empty() {
        s.push_str("  No picks cleared the score threshold.\n\n");
        return s;
    }

    s.push_str(&format!(
        "  {:>4}  {:<14}  {:>6}  {:>8}  {:>8}  {:>7}  {:>7}  {}\n",
        "Rank", "Ticker", "Score", "Mom%", "Fund%", "1W%", "1M%", "Industry"
    ));
    s.push_str("  ─────────────────────────────────────────────────────────────────────────────────────────────\n");

    for (i, pick) in picks.iter().enumerate() {
        let (w1, m1) = recent_returns(cache, &pick.ticker, today);
        let w1_str = w1.map(|v| format!("{:>+.1}%", v)).unwrap_or_else(|| "   n/a".into());
        let m1_str = m1.map(|v| format!("{:>+.1}%", v)).unwrap_or_else(|| "   n/a".into());

        let score_colour = if pick.composite >= 70.0 {
            "\x1b[32m"
        } else if pick.composite >= 60.0 {
            "\x1b[33m"
        } else {
            "\x1b[0m"
        };

        s.push_str(&format!(
            "  {:>4}  {:<14}  {}{:>6.1}\x1b[0m  {:>8.1}  {:>8.1}  {:>7}  {:>7}  {}\n",
            i + 1,
            pick.ticker,
            score_colour,
            pick.composite,
            pick.momentum_contrib,
            pick.fundamental_contrib,
            w1_str,
            m1_str,
            pick.industry,
        ));
    }
    s.push('\n');
    s
}

async fn format_market_context(cache: &Cache, today: NaiveDate) -> String {
    let yahoo = YahooFinance::new(cache.clone());
    let mut s = String::new();
    s.push_str("  \x1b[1mMarket Context\x1b[0m  (live — fetched now)\n");
    s.push_str("  ─────────────────────────────────────────\n");

    let five_days_ago = today - Duration::days(7);

    for (label, ticker) in &[("Nifty50", "^NSEI"), ("S&P500", "^GSPC")] {
        // Force-fetch today's bar so we always get the latest price
        let (_, latest) = yahoo.fetch_today_bar(ticker, today).await;
        let prior_bars = cache.get_price_bars(ticker, five_days_ago, today - Duration::days(1))
            .unwrap_or_default();
        let prior = prior_bars.first().map(|b| b.adj_close).unwrap_or(0.0);

        if latest > 0.0 && prior > 0.0 {
            let chg   = (latest - prior) / prior * 100.0;
            let arrow = if chg >= 0.0 { "\x1b[32m↑" } else { "\x1b[31m↓" };
            s.push_str(&format!(
                "  {:>8}  {:>10.2}   {}{:>+.2}%\x1b[0m vs 5d ago\n",
                label, latest, arrow, chg
            ));
        } else {
            s.push_str(&format!("  {:>8}  no data\n", label));
        }
    }

    // USD/INR — also force-refresh
    let (_, usdinr) = yahoo.fetch_today_bar("USDINR=X", today).await;
    if usdinr > 0.0 {
        s.push_str(&format!("  {:>8}  {:>10.4}\n", "USD/INR", usdinr));
    } else {
        s.push_str(&format!("  {:>8}  no data\n", "USD/INR"));
    }

    s.push('\n');
    s
}

fn format_portfolio_section(cache: &Cache, today: NaiveDate, hold_days: u32) -> String {
    let engine = PaperTradingEngine::new(cache.clone());
    let mut s = String::new();

    match engine.load_portfolio() {
        Ok(Some(portfolio)) => {
            s.push_str("  \x1b[1mPaper Portfolio Status\x1b[0m\n");
            s.push_str("  ─────────────────────────────────────────\n");

            let days_held = portfolio.days_since_rebalance(today);
            let total_val = portfolio.total_value();
            let inception_val = portfolio.cash
                + portfolio.positions.values().map(|p| p.shares * p.entry_price).sum::<f64>();
            let total_pnl = (total_val / inception_val.max(1.0) - 1.0) * 100.0;

            s.push_str(&format!(
                "  Positions: {}   Days since rebalance: {}   Total P&L: {:>+.2}%\n",
                portfolio.positions.len(),
                days_held,
                total_pnl,
            ));

            // Show open positions briefly
            let mut positions: Vec<_> = portfolio.positions.values().collect();
            positions.sort_by(|a, b| b.unrealised_pnl.partial_cmp(&a.unrealised_pnl).unwrap_or(std::cmp::Ordering::Equal));
            for pos in &positions {
                let days = (today - pos.entry_date).num_days();
                let colour = if pos.unrealised_pnl >= 0.0 { "\x1b[32m" } else { "\x1b[31m" };
                s.push_str(&format!(
                    "  {:<14}  entry {:>8.2}  current {:>8.2}  {:>3}d  {}P&L {:>+.2}%\x1b[0m\n",
                    pos.ticker, pos.entry_price, pos.current_price, days, colour, pos.unrealised_pnl,
                ));
            }

            // Rebalance alert
            if portfolio.rebalance_due(today) {
                s.push_str("\n  \x1b[1m\x1b[33m⚠  REBALANCE DUE — new picks ready. Run --paper-update to execute.\x1b[0m\n");
            } else {
                let days_left = hold_days as i64 - days_held;
                s.push_str(&format!(
                    "\n  Next rebalance in {} day(s).\n", days_left.max(0)
                ));
            }
        }
        Ok(None) => {
            s.push_str("  No paper portfolio. Run --paper-update to initialise.\n");
        }
        Err(e) => {
            warn!("Could not load paper portfolio: {e}");
        }
    }

    s.push('\n');
    s
}

// ── Price return helper ────────────────────────────────────────────────────────

fn recent_returns(cache: &Cache, ticker: &str, today: NaiveDate) -> (Option<f64>, Option<f64>) {
    let from = today - Duration::days(35);
    let bars = match cache.get_price_bars(ticker, from, today) {
        Ok(b) if !b.is_empty() => b,
        _ => return (None, None),
    };

    let latest = bars.last().map(|b| b.adj_close).unwrap_or(0.0);
    if latest <= 0.0 {
        return (None, None);
    }

    // 1-week: last 6 bars, compare first to last
    let week_change = if bars.len() >= 6 {
        let prior = bars[bars.len().saturating_sub(6)].adj_close;
        if prior > 0.0 { Some((latest - prior) / prior * 100.0) } else { None }
    } else {
        None
    };

    // 1-month: ~21 trading days
    let month_change = if bars.len() >= 21 {
        let prior = bars[bars.len().saturating_sub(21)].adj_close;
        if prior > 0.0 { Some((latest - prior) / prior * 100.0) } else { None }
    } else {
        None
    };

    (week_change, month_change)
}

// ── ANSI strip for plain-text file ────────────────────────────────────────────

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // consume escape sequence until 'm'
            while let Some(nc) = chars.next() {
                if nc == 'm' { break; }
            }
        } else {
            out.push(c);
        }
    }
    out
}
