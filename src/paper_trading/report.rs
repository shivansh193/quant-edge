use chrono::NaiveDate;

use super::engine::PaperPortfolio;

/// Print a formatted paper portfolio status table.
pub fn print_portfolio_status(portfolio: &PaperPortfolio, today: NaiveDate) {
    let total_value    = portfolio.total_value();
    let inception_val  = portfolio.cash
        + portfolio.positions.values().map(|p| p.shares * p.entry_price).sum::<f64>();
    let total_pnl_pct  = (total_value / inception_val.max(1.0) - 1.0) * 100.0;
    let days_held      = (today - portfolio.inception_date).num_days();

    println!();
    println!("\x1b[1m\x1b[97m══════════════════════════════════════════════\x1b[0m");
    println!("\x1b[1m\x1b[97m  PAPER PORTFOLIO — {}\x1b[0m", portfolio.strategy_spec.name);
    println!("\x1b[1m\x1b[97m══════════════════════════════════════════════\x1b[0m");
    println!();
    println!("  Inception Date       {}", portfolio.inception_date);
    println!("  Last Rebalance       {}", portfolio.last_rebalance_date);
    println!("  Days Since Rebalance {}", portfolio.days_since_rebalance(today));
    println!("  Rebalance Due        {}", if portfolio.rebalance_due(today) { "\x1b[33mYES\x1b[0m" } else { "no" });
    println!("  Holding Period       {} days", portfolio.strategy_spec.holding_period_days);
    println!();
    println!("  \x1b[1mPortfolio Summary\x1b[0m");
    println!("  ─────────────────────────────────────────");
    println!("  Total Value          {:>14.2}", total_value);
    println!("  Cash                 {:>14.2}", portfolio.cash);
    let pos_value: f64 = portfolio.positions.values().map(|p| p.shares * p.current_price).sum();
    println!("  Invested             {:>14.2}", pos_value);
    println!("  Total P&L            {:>13.2}%", total_pnl_pct);
    println!("  Days Running         {:>14}", days_held);
    println!();

    if portfolio.positions.is_empty() {
        println!("  No open positions.");
    } else {
        println!("  \x1b[1mOpen Positions\x1b[0m");
        println!("  ─────────────────────────────────────────────────────────────────");
        println!(
            "  {:>8}  {:>9}  {:>9}  {:>9}  {:>8}  {:>10}",
            "Ticker", "Entry", "Current", "Shares", "Days", "Unreal P&L"
        );
        println!("  ─────────────────────────────────────────────────────────────────");

        let mut positions: Vec<_> = portfolio.positions.values().collect();
        positions.sort_by(|a, b| {
            b.unrealised_pnl.partial_cmp(&a.unrealised_pnl).unwrap_or(std::cmp::Ordering::Equal)
        });

        for pos in &positions {
            let days = (today - pos.entry_date).num_days();
            let pnl_colour = if pos.unrealised_pnl >= 0.0 {
                "\x1b[32m"
            } else {
                "\x1b[31m"
            };
            println!(
                "  {:>8}  {:>9.2}  {:>9.2}  {:>9.2}  {:>8}  {}{:>+9.2}%\x1b[0m",
                pos.ticker,
                pos.entry_price,
                pos.current_price,
                pos.shares,
                days,
                pnl_colour,
                pos.unrealised_pnl,
            );
        }
    }

    println!();
}
