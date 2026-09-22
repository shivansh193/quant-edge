//! XIRR: the annualised internal rate of return of a series of dated,
//! irregularly-spaced cash flows. The standard way to measure "what did I
//! actually earn" when you added and withdrew money at different times,
//! which a simple total-return percentage can't do correctly.
//!
//! Convention: outflows (money you put in — buys) are negative, inflows
//! (money you got back — sells, dividends, or the current market value of
//! what you still hold, dated today) are positive.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CashFlow {
    pub date: NaiveDate,
    pub amount: f64,
}

impl CashFlow {
    pub fn new(date: NaiveDate, amount: f64) -> Self {
        Self { date, amount }
    }
}

const MAX_ITERATIONS: usize = 100;
const TOLERANCE: f64 = 1e-7;

/// Net present value of `flows` at annual rate `rate`, discounted from the
/// earliest flow's date.
fn npv(flows: &[CashFlow], t0: NaiveDate, rate: f64) -> f64 {
    flows
        .iter()
        .map(|f| {
            let years = (f.date - t0).num_days() as f64 / 365.0;
            f.amount / (1.0 + rate).powf(years)
        })
        .sum()
}

/// Derivative of `npv` with respect to `rate`, for Newton's method.
fn npv_derivative(flows: &[CashFlow], t0: NaiveDate, rate: f64) -> f64 {
    flows
        .iter()
        .map(|f| {
            let years = (f.date - t0).num_days() as f64 / 365.0;
            if years == 0.0 {
                0.0
            } else {
                -years * f.amount / (1.0 + rate).powf(years + 1.0)
            }
        })
        .sum()
}

/// Solve for the annualised rate that makes the flows' NPV zero.
///
/// Needs at least one negative and one positive flow (otherwise there is no
/// rate that can zero the NPV, and the answer would be meaningless). Uses
/// Newton's method from a 10% starting guess, falling back to bisection over
/// [-0.99, 10.0] if Newton doesn't converge (it can diverge for pathological
/// cash-flow patterns) or drifts outside that sane range.
pub fn xirr(flows: &[CashFlow]) -> Option<f64> {
    if flows.len() < 2 {
        return None;
    }
    let has_negative = flows.iter().any(|f| f.amount < 0.0);
    let has_positive = flows.iter().any(|f| f.amount > 0.0);
    if !has_negative || !has_positive {
        return None;
    }
    let t0 = flows.iter().map(|f| f.date).min()?;

    // Newton's method.
    let mut rate = 0.10;
    for _ in 0..MAX_ITERATIONS {
        let value = npv(flows, t0, rate);
        if value.abs() < TOLERANCE {
            return Some(rate);
        }
        let deriv = npv_derivative(flows, t0, rate);
        if deriv.abs() < 1e-12 {
            break;
        }
        let next = rate - value / deriv;
        if !next.is_finite() || next <= -0.99 || next > 100.0 {
            break; // diverged - fall through to bisection
        }
        rate = next;
    }

    // Bisection fallback: robust, just slower. Requires a sign change across
    // the bracket, which isn't guaranteed for every cash-flow pattern.
    let (mut lo, mut hi) = (-0.99, 10.0);
    let (mut f_lo, f_hi) = (npv(flows, t0, lo), npv(flows, t0, hi));
    if f_lo.is_nan() || f_hi.is_nan() || f_lo.signum() == f_hi.signum() {
        return None;
    }
    for _ in 0..200 {
        let mid = (lo + hi) / 2.0;
        let f_mid = npv(flows, t0, mid);
        if f_mid.abs() < TOLERANCE {
            return Some(mid);
        }
        if f_mid.signum() == f_lo.signum() {
            lo = mid;
            f_lo = f_mid;
        } else {
            hi = mid;
        }
    }
    Some((lo + hi) / 2.0)
}

/// XIRR of a strategy vs. simply buying and holding the benchmark over the
/// same flows' dates and magnitudes (same money in, same days invested).
/// Returns `(strategy_xirr, benchmark_xirr)`.
pub fn xirr_vs_benchmark(
    flows: &[CashFlow],
    benchmark_price_at: impl Fn(NaiveDate) -> Option<f64>,
) -> Option<(f64, f64)> {
    let strategy = xirr(flows)?;

    // Same-dated flows, but every outflow buys the benchmark at its price
    // that day and the final flow is valued at the benchmark's own return.
    let mut units = 0.0;
    let mut bench_flows: Vec<CashFlow> = Vec::new();
    let (last_flow, rest) = flows.split_last()?;
    for f in rest {
        let px = benchmark_price_at(f.date)?;
        if f.amount < 0.0 {
            units += -f.amount / px;
        }
        bench_flows.push(*f);
    }
    let final_px = benchmark_price_at(last_flow.date)?;
    bench_flows.push(CashFlow::new(last_flow.date, units * final_px));
    let benchmark = xirr(&bench_flows)?;
    Some((strategy, benchmark))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn a_simple_one_year_10_percent_return() {
        let flows = vec![
            CashFlow::new(d("2023-01-01"), -1000.0),
            CashFlow::new(d("2024-01-01"), 1100.0),
        ];
        let r = xirr(&flows).unwrap();
        assert!((r - 0.10).abs() < 1e-4, "{r}");
    }

    #[test]
    fn doubling_in_a_year_is_100_percent() {
        let flows = vec![
            CashFlow::new(d("2023-01-01"), -1000.0),
            CashFlow::new(d("2024-01-01"), 2000.0),
        ];
        let r = xirr(&flows).unwrap();
        assert!((r - 1.0).abs() < 1e-3, "{r}");
    }

    #[test]
    fn a_loss_gives_a_negative_rate() {
        let flows = vec![
            CashFlow::new(d("2023-01-01"), -1000.0),
            CashFlow::new(d("2024-01-01"), 500.0),
        ];
        let r = xirr(&flows).unwrap();
        assert!(r < 0.0 && r > -1.0, "{r}");
    }

    #[test]
    fn irregular_deposits_match_an_independently_computed_case() {
        // Invest 1000 at t0, another 500 six months later, withdraw 1650 one
        // year after the first investment. Expected rate (≈12.05%) computed
        // independently via plain bisection in Python, not just by trusting
        // this same Newton's-method implementation.
        let flows = vec![
            CashFlow::new(d("2023-01-01"), -1000.0),
            CashFlow::new(d("2023-07-01"), -500.0),
            CashFlow::new(d("2024-01-01"), 1650.0),
        ];
        let r = xirr(&flows).unwrap();
        assert!((r - 0.1205).abs() < 0.002, "{r}");
    }

    #[test]
    fn zero_return_gives_zero_rate() {
        let flows = vec![CashFlow::new(d("2023-01-01"), -1000.0), CashFlow::new(d("2024-01-01"), 1000.0)];
        let r = xirr(&flows).unwrap();
        assert!(r.abs() < 1e-4, "{r}");
    }

    #[test]
    fn needs_both_a_negative_and_a_positive_flow() {
        assert!(xirr(&[CashFlow::new(d("2023-01-01"), -1000.0), CashFlow::new(d("2024-01-01"), -500.0)]).is_none());
        assert!(xirr(&[CashFlow::new(d("2023-01-01"), 1000.0), CashFlow::new(d("2024-01-01"), 500.0)]).is_none());
        assert!(xirr(&[CashFlow::new(d("2023-01-01"), -1000.0)]).is_none());
        assert!(xirr(&[]).is_none());
    }

    #[test]
    fn is_independent_of_input_order() {
        let flows_a = vec![
            CashFlow::new(d("2023-01-01"), -1000.0),
            CashFlow::new(d("2023-07-01"), -500.0),
            CashFlow::new(d("2024-01-01"), 1650.0),
        ];
        let mut flows_b = flows_a.clone();
        flows_b.reverse();
        assert!((xirr(&flows_a).unwrap() - xirr(&flows_b).unwrap()).abs() < 1e-9);
    }

    #[test]
    fn many_small_deposits_stays_numerically_stable() {
        let mut flows = Vec::new();
        let mut date = d("2020-01-01");
        for _ in 0..48 {
            flows.push(CashFlow::new(date, -100.0));
            date += chrono::Duration::days(30);
        }
        flows.push(CashFlow::new(date, 5500.0)); // paid in 4800, got back 5500
        let r = xirr(&flows).unwrap();
        assert!(r.is_finite() && r > -1.0 && r < 5.0, "{r}");
    }

    #[test]
    fn benchmark_comparison_uses_the_same_flow_dates_and_sizes() {
        let flows = vec![
            CashFlow::new(d("2023-01-01"), -1000.0),
            CashFlow::new(d("2024-01-01"), 1300.0), // strategy: +30%
        ];
        // Benchmark only rose 10% over the same window.
        let bench_price = |date: NaiveDate| -> Option<f64> {
            if date == d("2023-01-01") { Some(100.0) } else if date == d("2024-01-01") { Some(110.0) } else { None }
        };
        let (strat, bench) = xirr_vs_benchmark(&flows, bench_price).unwrap();
        assert!((strat - 0.30).abs() < 1e-3);
        assert!((bench - 0.10).abs() < 1e-3);
        assert!(strat > bench, "strategy beat the benchmark in this fixture");
    }
}
