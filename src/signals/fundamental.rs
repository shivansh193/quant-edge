use crate::data::FundamentalSnapshot;
use super::{MarketData, Signal, clamp_signal, zscore_signal};

/// Combines four quality/value/growth sub-signals:
///   1. Revenue growth YoY vs industry median
///   2. Net margin vs industry median
///   3. P/E relative to sector (low = value, context-dependent)
///   4. Piotroski F-score proxy (from available metrics)
pub struct FundamentalSignal;

impl Signal for FundamentalSignal {
    fn name(&self) -> &str {
        "Fundamental"
    }

    fn compute(&self, _ticker: &str, data: &MarketData) -> f64 {
        let Some(fund) = &data.fundamentals else {
            return 0.0;
        };

        let peer_snaps: Vec<&FundamentalSnapshot> = data.peer_fundamentals.values().collect();

        // ── 1. Revenue growth relative to peers ──────────────────────────────
        let rev_score = match fund.revenue_cagr_3yr {
            Some(cagr) => {
                let peer_cagrs: Vec<f64> = peer_snaps
                    .iter()
                    .filter_map(|p| p.revenue_cagr_3yr)
                    .collect();
                if peer_cagrs.is_empty() {
                    // Absolute: positive CAGR is good
                    clamp_signal(cagr * 3.0) // e.g. 30% CAGR → +0.9
                } else {
                    zscore_signal(cagr, &peer_cagrs, true)
                }
            }
            None => 0.0,
        };

        // ── 2. Net margin relative to peers ──────────────────────────────────
        let margin_score = match fund.net_margin_pct {
            Some(margin) => {
                let peer_margins: Vec<f64> = peer_snaps
                    .iter()
                    .filter_map(|p| p.net_margin_pct)
                    .collect();
                if peer_margins.is_empty() {
                    clamp_signal(margin / 30.0) // 30% net margin → +1
                } else {
                    zscore_signal(margin, &peer_margins, true)
                }
            }
            None => 0.0,
        };

        // ── 3. P/B ratio relative to peers (lower is better = deep value) ───
        let pb_score = match fund.price_to_book {
            Some(pb) if pb > 0.0 => {
                let peer_pbs: Vec<f64> = peer_snaps
                    .iter()
                    .filter_map(|p| p.price_to_book.filter(|&v| v > 0.0))
                    .collect();
                if peer_pbs.is_empty() {
                    // Low P/B is value; P/B < 1 is strongly positive
                    clamp_signal(1.5 / pb.max(0.1) - 1.0)
                } else {
                    zscore_signal(pb, &peer_pbs, false) // lower P/B = better
                }
            }
            _ => 0.0,
        };

        // ── 4. Piotroski F-score proxy ────────────────────────────────────────
        // Full F-score needs 9 signals; we compute from available metrics.
        let f_score = piotroski_proxy(fund);
        // F-score: 0–9, normalise to [-1, +1]: (score/9)*2 - 1
        let f_signal = (f_score as f64 / 9.0) * 2.0 - 1.0;

        // Combine: revenue 35%, margin 35%, P/B 15%, Piotroski 15%
        let raw = 0.35 * rev_score + 0.35 * margin_score + 0.15 * pb_score + 0.15 * f_signal;
        clamp_signal(raw)
    }
}

// ── Full 9-point Piotroski F-score ────────────────────────────────────────────
// Adapted to Yahoo Finance fields. Each criterion = 1 point.
//
// Profitability (4 pts):  ROA>0, CFO>0, net margin>0, gross margin quality
// Leverage     (3 pts):  D/E<1, D/E<0.5, revenue scale (no-dilution proxy)
// Efficiency   (2 pts):  revenue growth, price momentum / asset turnover proxy

fn piotroski_proxy(fund: &FundamentalSnapshot) -> u8 {
    let mut score = 0u8;

    // Profitability —————————————————————————————————————————————————————————
    // 1. Return on assets > 0
    if fund.return_on_assets.map_or(false, |v| v > 0.0) { score += 1; }
    // 2. Operating cash flow > 0
    if fund.operating_cashflow.map_or(false, |v| v > 0.0) { score += 1; }
    // 3. Positive net income (net margin > 0)
    if fund.net_margin_pct.map_or(false, |v| v > 0.0) { score += 1; }
    // 4. Accruals quality: gross margin > 25% signals earnings quality
    if fund.gross_profit_margin.map_or(false, |v| v > 25.0) { score += 1; }

    // Leverage / liquidity ————————————————————————————————————————————————
    // 5. D/E < 1.0 (stored as percentage, so < 100)
    if fund.debt_to_equity.map_or(false, |v| v < 100.0) { score += 1; }
    // 6. D/E < 0.5 — conservatively financed
    if fund.debt_to_equity.map_or(false, |v| v < 50.0)  { score += 1; }
    // 7. Revenue scale > $1B — no-dilution / moat proxy
    if fund.revenue_ttm.map_or(false, |v| v > 1_000_000_000.0) { score += 1; }

    // Operating efficiency ————————————————————————————————————————————————
    // 8. Revenue CAGR > 5% — improving asset turnover proxy
    if fund.revenue_cagr_3yr.map_or(false, |v| v > 0.05) { score += 1; }
    // 9. Positive price momentum — market confirmation of operational improvement
    if fund.price_return_12m_1m.map_or(false, |v| v > 0.0) { score += 1; }

    score.min(9)
}
