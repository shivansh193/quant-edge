use anyhow::{Context, Result};
use chrono::{Duration, NaiveDate};
use crate::data::{cache::Cache, yahoo::YahooFinance};
use crate::paper_trading::engine::PaperTradingEngine;

use super::holding_period_days;

/// Run the evening mark-to-market report. Returns formatted output (also saved to logs/).
/// Always force-fetches today's bar from Yahoo Finance so prices are live regardless
/// of when --morning was last run.
pub async fn run_evening(cache: Cache, today: NaiveDate) -> Result<String> {
    let hold_days = holding_period_days();

    let engine  = PaperTradingEngine::new(cache.clone());
    let yahoo   = YahooFinance::new(cache.clone());
    let portfolio = match engine.load_portfolio()? {
        Some(p) => p,
        None => {
            let msg = "No paper portfolio found. Run --paper-update to initialise.\n";
            return Ok(msg.to_string());
        }
    };

    let mut out = String::new();
    out.push_str(&format!(
        "\n\x1b[1m\x1b[97m══════════════════════════════════════════════════════════════\x1b[0m\n"
    ));
    out.push_str(&format!(
        "\x1b[1m\x1b[97m  EVENING REPORT — {}\x1b[0m\n",
        today
    ));
    out.push_str(&format!(
        "\x1b[1m\x1b[97m══════════════════════════════════════════════════════════════\x1b[0m\n\n"
    ));

    // ── 1. Mark positions to market (force-fresh from Yahoo) ─────────────────

    let inception_val = portfolio.cash
        + portfolio.positions.values().map(|p| p.shares * p.entry_price).sum::<f64>();

    let mut marked_positions = portfolio.positions.clone();
    let mut today_pnl_vec: Vec<(String, f64, bool)> = Vec::new(); // (ticker, today_move%, flagged)

    for (ticker, pos) in &mut marked_positions {
        // fetch_today_bar always hits Yahoo for the latest bar, updating cache as a side-effect.
        // This means running --evening after --morning on the same day will always show
        // today's actual close (or live price during market hours), not the morning snapshot.
        let (_open, latest_price) = yahoo.fetch_today_bar(ticker, today).await;

        // For "today's move" we compare against yesterday's close from cache.
        let prev_price = cache
            .get_price_bars(ticker, today - Duration::days(7), today - Duration::days(1))
            .unwrap_or_default()
            .last()
            .map(|b| b.adj_close)
            .unwrap_or(0.0);

        if latest_price > 0.0 {
            pos.current_price  = latest_price;
            pos.unrealised_pnl = (latest_price - pos.entry_price) / pos.entry_price * 100.0;

            let today_move = if prev_price > 0.0 {
                (latest_price - prev_price) / prev_price * 100.0
            } else {
                0.0
            };
            let flagged = today_move.abs() > 3.0;
            today_pnl_vec.push((ticker.clone(), today_move, flagged));
        } else {
            today_pnl_vec.push((ticker.clone(), 0.0, false));
        }
    }

    // Sort by today's move descending
    today_pnl_vec.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let stale_threshold = (hold_days as f64 * 1.5) as i64;

    // ── 2. Positions table ────────────────────────────────────────────────────

    out.push_str("  \x1b[1mOpen Positions — End of Day\x1b[0m\n");
    out.push_str("  ──────────────────────────────────────────────────────────────────────────────────\n");
    out.push_str(&format!(
        "  {:>4}  {:<14}  {:>9}  {:>9}  {:>9}  {:>7}  {:>8}  {}\n",
        "Rank", "Ticker", "Entry", "Current", "Shares", "Days", "P&L%", "Today"
    ));
    out.push_str("  ──────────────────────────────────────────────────────────────────────────────────\n");

    let mut positions_sorted: Vec<_> = marked_positions.values().collect();
    positions_sorted.sort_by(|a, b| {
        b.unrealised_pnl.partial_cmp(&a.unrealised_pnl).unwrap_or(std::cmp::Ordering::Equal)
    });

    let move_map: std::collections::HashMap<String, (f64, bool)> =
        today_pnl_vec.iter().map(|(t, m, f)| (t.clone(), (*m, *f))).collect();

    for (rank, pos) in positions_sorted.iter().enumerate() {
        let days = (today - pos.entry_date).num_days();
        let pnl_col = if pos.unrealised_pnl >= 0.0 { "\x1b[32m" } else { "\x1b[31m" };
        let (today_move, flagged) = move_map.get(&pos.ticker).copied().unwrap_or((0.0, false));
        let today_col = if today_move >= 0.0 { "\x1b[32m" } else { "\x1b[31m" };
        let flag_str = if flagged { " \x1b[33m⚠\x1b[0m" } else { "  " };
        let stale_str = if days > stale_threshold { " \x1b[35mSTALE\x1b[0m" } else { "" };

        out.push_str(&format!(
            "  {:>4}  {:<14}  {:>9.2}  {:>9.2}  {:>9.2}  {:>7}  {}{:>+7.2}%\x1b[0m  {}{:>+.2}%{}{}\n",
            rank + 1,
            pos.ticker,
            pos.entry_price,
            pos.current_price,
            pos.shares,
            days,
            pnl_col,
            pos.unrealised_pnl,
            today_col,
            today_move,
            flag_str,
            stale_str,
        ));
    }
    out.push('\n');

    // ── 3. Summary ────────────────────────────────────────────────────────────

    let total_val: f64 = portfolio.cash
        + marked_positions.values().map(|p| p.shares * p.current_price).sum::<f64>();
    let total_pnl = (total_val / inception_val.max(1.0) - 1.0) * 100.0;

    let best  = today_pnl_vec.first();
    let worst = today_pnl_vec.last();

    out.push_str("  \x1b[1mSummary\x1b[0m\n");
    out.push_str("  ─────────────────────────────────────────\n");
    out.push_str(&format!(
        "  Total portfolio value : {:>12.2}\n", total_val
    ));
    out.push_str(&format!(
        "  P&L since inception   : {:>+11.2}%\n", total_pnl
    ));
    if let Some((ticker, mv, _)) = best {
        out.push_str(&format!("  Best today            : {} ({:>+.2}%)\n", ticker, mv));
    }
    if let Some((ticker, mv, _)) = worst {
        out.push_str(&format!("  Worst today           : {} ({:>+.2}%)\n", ticker, mv));
    }

    let flagged_count = today_pnl_vec.iter().filter(|(_, _, f)| *f).count();
    if flagged_count > 0 {
        out.push_str(&format!(
            "\n  \x1b[33m⚠  {} position(s) moved >±3% today — review warranted.\x1b[0m\n",
            flagged_count
        ));
    }

    let stale_count = positions_sorted.iter()
        .filter(|p| (today - p.entry_date).num_days() > stale_threshold)
        .count();
    if stale_count > 0 {
        out.push_str(&format!(
            "  \x1b[35mSTALE: {} position(s) held beyond {:.0}× holding period ({} days).\x1b[0m\n",
            stale_count, 1.5, stale_threshold
        ));
    }

    out.push_str(&format!(
        "\n\x1b[1m\x1b[97m══════════════════════════════════════════════════════════════\x1b[0m\n"
    ));

    // ── 4. Save to file ───────────────────────────────────────────────────────
    let plain = strip_ansi(&out);
    std::fs::create_dir_all("logs").ok();
    let log_path = format!("logs/evening_{}.txt", today.format("%Y%m%d"));
    std::fs::write(&log_path, &plain)
        .with_context(|| format!("Failed to write {}", log_path))?;

    Ok(out)
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            while let Some(nc) = chars.next() {
                if nc == 'm' { break; }
            }
        } else {
            out.push(c);
        }
    }
    out
}
