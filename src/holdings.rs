//! Import real trades from a broker export, reconcile them against the
//! model's current picks, and compute actual (XIRR) performance.
//!
//! No specific broker's export format is assumed — brokers differ too much
//! (Zerodha, Groww, Schwab, Fidelity all use their own columns) to guess at
//! correctly without seeing a real file. Instead this defines one small,
//! documented CSV schema you reformat into:
//!
//! ```text
//! date,ticker,side,quantity,price,fees
//! 2024-01-15,AAPL,BUY,10,185.50,1.00
//! 2024-06-01,AAPL,SELL,4,210.00,1.00
//! ```
//!
//! `fees` is optional (defaults to 0). `side` is case-insensitive. Most
//! spreadsheet tools and brokers' own CSV exports can be reshaped into this
//! with a few column renames.

use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::xirr::{xirr, CashFlow};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trade {
    pub date: NaiveDate,
    pub ticker: String,
    pub side: Side,
    pub quantity: f64,
    pub price: f64,
    pub fees: f64,
}

impl Trade {
    /// Signed cash impact: negative for a buy (money out), positive for a
    /// sell (money in), fees always reduce what you keep either way.
    pub fn cash_flow(&self) -> f64 {
        let gross = self.quantity * self.price;
        match self.side {
            Side::Buy => -(gross + self.fees),
            Side::Sell => gross - self.fees,
        }
    }
}

/// Parse the documented CSV schema. Header required; column order flexible
/// (matched by name), extra columns ignored. Every row must parse cleanly —
/// a partially-garbled trade history is worse than an explicit error.
pub fn parse_trades_csv(text: &str) -> Result<Vec<Trade>> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let header = lines.next().context("empty CSV: no header row")?;
    let cols: Vec<String> = header.split(',').map(|c| c.trim().to_lowercase()).collect();
    let idx = |name: &str| -> Result<usize> {
        cols.iter().position(|c| c == name).with_context(|| format!("CSV is missing a '{name}' column"))
    };
    let (i_date, i_ticker, i_side, i_qty, i_price) =
        (idx("date")?, idx("ticker")?, idx("side")?, idx("quantity")?, idx("price")?);
    let i_fees = cols.iter().position(|c| c == "fees");

    let mut trades = Vec::new();
    for (line_no, line) in lines.enumerate() {
        let fields: Vec<&str> = line.split(',').map(|f| f.trim()).collect();
        let get = |i: usize| -> Result<&str> {
            fields.get(i).copied().with_context(|| format!("row {} (line {}) is missing a column", line_no + 2, line_no + 2))
        };
        let date: NaiveDate = get(i_date)?.parse().with_context(|| format!("row {}: bad date", line_no + 2))?;
        let ticker = get(i_ticker)?.to_uppercase();
        let side = match get(i_side)?.to_uppercase().as_str() {
            "BUY" | "B" => Side::Buy,
            "SELL" | "S" => Side::Sell,
            other => bail!("row {}: side must be BUY or SELL, got '{other}'", line_no + 2),
        };
        let quantity: f64 = get(i_qty)?.parse().with_context(|| format!("row {}: bad quantity", line_no + 2))?;
        let price: f64 = get(i_price)?.parse().with_context(|| format!("row {}: bad price", line_no + 2))?;
        anyhow::ensure!(quantity > 0.0, "row {}: quantity must be positive", line_no + 2);
        anyhow::ensure!(price > 0.0, "row {}: price must be positive", line_no + 2);
        let fees = match i_fees.and_then(|i| fields.get(i)) {
            Some(s) if !s.trim().is_empty() => s.trim().parse().with_context(|| format!("row {}: bad fees", line_no + 2))?,
            _ => 0.0,
        };
        trades.push(Trade { date, ticker, side, quantity, price, fees });
    }
    Ok(trades)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Holding {
    pub quantity: f64,
    /// Weighted-average cost per share of the CURRENT quantity (not FIFO
    /// lots): simpler, and matches how most brokers show "average cost".
    pub avg_cost: f64,
    pub realized_pnl: f64,
}

/// Fold trades into current per-ticker holdings, in date order. A sell
/// realises P&L against the running average cost; it does not change the
/// average cost of what remains.
pub fn compute_holdings(trades: &[Trade]) -> HashMap<String, Holding> {
    let mut sorted: Vec<&Trade> = trades.iter().collect();
    sorted.sort_by_key(|t| t.date);

    let mut out: HashMap<String, Holding> = HashMap::new();
    for t in sorted {
        let h = out.entry(t.ticker.clone()).or_default();
        match t.side {
            Side::Buy => {
                let total_cost = h.avg_cost * h.quantity + t.quantity * t.price + t.fees;
                h.quantity += t.quantity;
                h.avg_cost = if h.quantity > 0.0 { total_cost / h.quantity } else { 0.0 };
            }
            Side::Sell => {
                let sell_qty = t.quantity.min(h.quantity);
                h.realized_pnl += sell_qty * (t.price - h.avg_cost) - t.fees;
                h.quantity -= sell_qty;
                if h.quantity <= 1e-9 {
                    h.quantity = 0.0;
                    h.avg_cost = 0.0;
                }
            }
        }
    }
    out.retain(|_, h| h.quantity > 1e-9 || h.realized_pnl != 0.0);
    out
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Reconciliation {
    /// Held and currently in the model's picks.
    pub in_both: Vec<String>,
    /// Held but the model does not currently pick it.
    pub only_held: Vec<String>,
    /// In the model's picks but you don't hold it.
    pub only_model: Vec<String>,
}

pub fn reconcile(held: &HashMap<String, Holding>, model_picks: &HashSet<String>) -> Reconciliation {
    let held_tickers: HashSet<String> = held.keys().cloned().collect();
    let mut r = Reconciliation {
        in_both: held_tickers.intersection(model_picks).cloned().collect(),
        only_held: held_tickers.difference(model_picks).cloned().collect(),
        only_model: model_picks.difference(&held_tickers).cloned().collect(),
    };
    r.in_both.sort();
    r.only_held.sort();
    r.only_model.sort();
    r
}

/// XIRR of the whole trade history: every buy/sell as its own cash flow,
/// plus one final flow for the CURRENT market value of whatever remains
/// held, dated `as_of`. Requires a price for every still-held ticker.
pub fn portfolio_xirr(
    trades: &[Trade],
    current_prices: &HashMap<String, f64>,
    as_of: NaiveDate,
) -> Result<Option<f64>> {
    let holdings = compute_holdings(trades);
    let mut flows: Vec<CashFlow> = trades.iter().map(|t| CashFlow::new(t.date, t.cash_flow())).collect();

    let mut remaining_value = 0.0;
    for (ticker, h) in &holdings {
        if h.quantity <= 0.0 {
            continue;
        }
        let px = current_prices.get(ticker).with_context(|| format!("no current price given for held ticker {ticker}"))?;
        remaining_value += h.quantity * px;
    }
    if remaining_value > 0.0 {
        flows.push(CashFlow::new(as_of, remaining_value));
    }
    Ok(xirr(&flows))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn parses_the_documented_schema() {
        let csv = "date,ticker,side,quantity,price,fees\n\
                    2024-01-15,AAPL,BUY,10,185.50,1.00\n\
                    2024-06-01,aapl,sell,4,210.00,1.00\n";
        let trades = parse_trades_csv(csv).unwrap();
        assert_eq!(trades.len(), 2);
        assert_eq!(trades[0].ticker, "AAPL");
        assert_eq!(trades[1].side, Side::Sell);
        assert_eq!(trades[1].ticker, "AAPL", "lowercase ticker/side normalised to upper");
    }

    #[test]
    fn fees_default_to_zero_when_the_column_is_absent_or_blank() {
        let csv = "date,ticker,side,quantity,price\n2024-01-15,AAPL,BUY,10,185.50\n";
        let trades = parse_trades_csv(csv).unwrap();
        assert_eq!(trades[0].fees, 0.0);

        let csv2 = "date,ticker,side,quantity,price,fees\n2024-01-15,AAPL,BUY,10,185.50,\n";
        let trades2 = parse_trades_csv(csv2).unwrap();
        assert_eq!(trades2[0].fees, 0.0);
    }

    #[test]
    fn column_order_does_not_matter() {
        let csv = "ticker,price,date,side,quantity\nMSFT,400.0,2024-02-01,BUY,5\n";
        let trades = parse_trades_csv(csv).unwrap();
        assert_eq!(trades[0].ticker, "MSFT");
        assert_eq!(trades[0].price, 400.0);
    }

    #[test]
    fn rejects_missing_columns_bad_values_and_unknown_side() {
        assert!(parse_trades_csv("date,ticker,side,quantity\n2024-01-01,X,BUY,1\n").is_err(), "missing price column");
        assert!(parse_trades_csv("date,ticker,side,quantity,price\nnotadate,X,BUY,1,10\n").is_err());
        assert!(parse_trades_csv("date,ticker,side,quantity,price\n2024-01-01,X,HOLD,1,10\n").is_err());
        assert!(parse_trades_csv("date,ticker,side,quantity,price\n2024-01-01,X,BUY,-1,10\n").is_err(), "negative quantity");
        assert!(parse_trades_csv("").is_err());
    }

    #[test]
    fn cash_flow_sign_and_fee_direction() {
        let buy = Trade { date: d("2024-01-01"), ticker: "X".into(), side: Side::Buy, quantity: 10.0, price: 100.0, fees: 1.0 };
        assert!((buy.cash_flow() + 1001.0).abs() < 1e-9);
        let sell = Trade { date: d("2024-01-01"), ticker: "X".into(), side: Side::Sell, quantity: 10.0, price: 100.0, fees: 1.0 };
        assert!((sell.cash_flow() - 999.0).abs() < 1e-9);
    }

    #[test]
    fn holdings_track_weighted_average_cost_across_multiple_buys() {
        let trades = vec![
            Trade { date: d("2024-01-01"), ticker: "X".into(), side: Side::Buy, quantity: 10.0, price: 100.0, fees: 0.0 },
            Trade { date: d("2024-02-01"), ticker: "X".into(), side: Side::Buy, quantity: 10.0, price: 200.0, fees: 0.0 },
        ];
        let h = compute_holdings(&trades);
        assert_eq!(h["X"].quantity, 20.0);
        assert!((h["X"].avg_cost - 150.0).abs() < 1e-9); // (1000+2000)/20
    }

    #[test]
    fn a_sell_realises_pnl_against_average_cost_without_changing_it() {
        let trades = vec![
            Trade { date: d("2024-01-01"), ticker: "X".into(), side: Side::Buy, quantity: 10.0, price: 100.0, fees: 0.0 },
            Trade { date: d("2024-06-01"), ticker: "X".into(), side: Side::Sell, quantity: 4.0, price: 150.0, fees: 2.0 },
        ];
        let h = compute_holdings(&trades);
        assert_eq!(h["X"].quantity, 6.0);
        assert!((h["X"].avg_cost - 100.0).abs() < 1e-9, "remaining shares keep the original cost basis");
        assert!((h["X"].realized_pnl - (4.0 * 50.0 - 2.0)).abs() < 1e-9);
    }

    #[test]
    fn fully_exited_positions_are_dropped_but_remembered_if_they_had_pnl() {
        let trades = vec![
            Trade { date: d("2024-01-01"), ticker: "X".into(), side: Side::Buy, quantity: 10.0, price: 100.0, fees: 0.0 },
            Trade { date: d("2024-06-01"), ticker: "X".into(), side: Side::Sell, quantity: 10.0, price: 150.0, fees: 0.0 },
        ];
        let h = compute_holdings(&trades);
        assert_eq!(h["X"].quantity, 0.0);
        assert!((h["X"].realized_pnl - 500.0).abs() < 1e-9, "closed position's P&L is still reported");
    }

    #[test]
    fn reconciliation_sorts_tickers_into_three_buckets() {
        let mut held = HashMap::new();
        held.insert("AAPL".to_string(), Holding { quantity: 10.0, ..Default::default() });
        held.insert("TSLA".to_string(), Holding { quantity: 5.0, ..Default::default() });
        let model: HashSet<String> = ["AAPL", "MSFT"].iter().map(|s| s.to_string()).collect();
        let r = reconcile(&held, &model);
        assert_eq!(r.in_both, vec!["AAPL"]);
        assert_eq!(r.only_held, vec!["TSLA"]);
        assert_eq!(r.only_model, vec!["MSFT"]);
    }

    #[test]
    fn portfolio_xirr_of_a_single_round_trip_matches_a_direct_return() {
        let trades = vec![
            Trade { date: d("2023-01-01"), ticker: "X".into(), side: Side::Buy, quantity: 10.0, price: 100.0, fees: 0.0 },
        ];
        let mut prices = HashMap::new();
        prices.insert("X".to_string(), 110.0);
        // Held for exactly 1 year, +10% -> XIRR should be ~10%.
        let r = portfolio_xirr(&trades, &prices, d("2024-01-01")).unwrap().unwrap();
        assert!((r - 0.10).abs() < 1e-3, "{r}");
    }

    #[test]
    fn portfolio_xirr_requires_a_price_for_every_still_held_ticker() {
        let trades = vec![Trade { date: d("2023-01-01"), ticker: "X".into(), side: Side::Buy, quantity: 10.0, price: 100.0, fees: 0.0 }];
        assert!(portfolio_xirr(&trades, &HashMap::new(), d("2024-01-01")).is_err());
    }

    #[test]
    fn portfolio_xirr_of_a_fully_closed_book_needs_no_current_prices() {
        let trades = vec![
            Trade { date: d("2023-01-01"), ticker: "X".into(), side: Side::Buy, quantity: 10.0, price: 100.0, fees: 0.0 },
            Trade { date: d("2024-01-01"), ticker: "X".into(), side: Side::Sell, quantity: 10.0, price: 120.0, fees: 0.0 },
        ];
        let r = portfolio_xirr(&trades, &HashMap::new(), d("2024-06-01")).unwrap().unwrap();
        assert!((r - 0.20).abs() < 1e-3, "{r}");
    }
}
