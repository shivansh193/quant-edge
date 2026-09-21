//! Information-coefficient statistics.
//!
//! IC is the cross-sectional correlation between a signal and the *subsequent*
//! return. Rank (Spearman) IC is the standard because it is robust to the fat
//! tails that dominate raw stock returns; Pearson is kept for comparison.

use serde::{Deserialize, Serialize};

/// Pearson correlation. Returns `None` for fewer than 2 finite pairs or when
/// either side has no variance.
pub fn pearson(xs: &[f64], ys: &[f64]) -> Option<f64> {
    let pairs: Vec<(f64, f64)> = xs
        .iter()
        .zip(ys)
        .filter(|(x, y)| x.is_finite() && y.is_finite())
        .map(|(&x, &y)| (x, y))
        .collect();
    let n = pairs.len();
    if n < 2 {
        return None;
    }
    let nf = n as f64;
    let mx = pairs.iter().map(|p| p.0).sum::<f64>() / nf;
    let my = pairs.iter().map(|p| p.1).sum::<f64>() / nf;
    let cov: f64 = pairs.iter().map(|(x, y)| (x - mx) * (y - my)).sum();
    let vx: f64 = pairs.iter().map(|(x, _)| (x - mx).powi(2)).sum();
    let vy: f64 = pairs.iter().map(|(_, y)| (y - my).powi(2)).sum();
    if vx < 1e-18 || vy < 1e-18 {
        return None;
    }
    Some((cov / (vx.sqrt() * vy.sqrt())).clamp(-1.0, 1.0))
}

/// 1-based average ranks (ties share the mean of the ranks they span).
pub fn ranks(values: &[f64]) -> Vec<f64> {
    let mut idx: Vec<usize> = (0..values.len()).collect();
    idx.sort_by(|&a, &b| {
        values[a]
            .partial_cmp(&values[b])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut out = vec![0.0; values.len()];
    let mut i = 0;
    while i < idx.len() {
        let mut j = i;
        while j + 1 < idx.len() && values[idx[j + 1]] == values[idx[i]] {
            j += 1;
        }
        let avg = (i + j) as f64 / 2.0 + 1.0;
        for &k in &idx[i..=j] {
            out[k] = avg;
        }
        i = j + 1;
    }
    out
}

/// Spearman rank correlation (Pearson on average ranks).
pub fn spearman(xs: &[f64], ys: &[f64]) -> Option<f64> {
    let (fx, fy): (Vec<f64>, Vec<f64>) = xs
        .iter()
        .zip(ys)
        .filter(|(x, y)| x.is_finite() && y.is_finite())
        .map(|(&x, &y)| (x, y))
        .unzip();
    if fx.len() < 2 {
        return None;
    }
    pearson(&ranks(&fx), &ranks(&fy))
}

/// Summary of an IC time series (one IC per rebalance / test date).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IcSummary {
    pub n: usize,
    pub mean: f64,
    pub std: f64,
    /// mean / (std / sqrt(n)) — a t-test against "no predictive power".
    /// Overlapping forward windows inflate this; treat as indicative only.
    pub t_stat: f64,
    /// IC information ratio: mean / std.
    pub ic_ir: f64,
    /// Fraction of periods with IC > 0.
    pub hit_rate: f64,
}

pub fn summarize(ics: &[f64]) -> IcSummary {
    let v: Vec<f64> = ics.iter().copied().filter(|x| x.is_finite()).collect();
    let n = v.len();
    if n == 0 {
        return IcSummary::default();
    }
    let nf = n as f64;
    let mean = v.iter().sum::<f64>() / nf;
    let std = if n > 1 {
        (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (nf - 1.0)).sqrt()
    } else {
        0.0
    };
    let (t_stat, ic_ir) = if std > 1e-12 {
        (mean / (std / nf.sqrt()), mean / std)
    } else {
        (0.0, 0.0)
    };
    let hit_rate = v.iter().filter(|x| **x > 0.0).count() as f64 / nf;
    IcSummary { n, mean, std, t_stat, ic_ir, hit_rate }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pearson_perfect_and_inverse() {
        let x = [1.0, 2.0, 3.0, 4.0];
        assert!((pearson(&x, &[2.0, 4.0, 6.0, 8.0]).unwrap() - 1.0).abs() < 1e-12);
        assert!((pearson(&x, &[8.0, 6.0, 4.0, 2.0]).unwrap() + 1.0).abs() < 1e-12);
    }

    #[test]
    fn pearson_none_when_no_variance_or_too_few() {
        assert!(pearson(&[1.0, 1.0, 1.0], &[1.0, 2.0, 3.0]).is_none());
        assert!(pearson(&[1.0], &[1.0]).is_none());
    }

    #[test]
    fn ranks_average_ties() {
        assert_eq!(ranks(&[10.0, 20.0, 20.0, 30.0]), vec![1.0, 2.5, 2.5, 4.0]);
    }

    #[test]
    fn spearman_is_rank_based_not_linear() {
        // Monotonic but wildly non-linear: Spearman = 1, Pearson < 1.
        let x = [1.0, 2.0, 3.0, 4.0, 5.0];
        let y = [1.0, 10.0, 100.0, 1_000.0, 100_000.0];
        assert!((spearman(&x, &y).unwrap() - 1.0).abs() < 1e-12);
        assert!(pearson(&x, &y).unwrap() < 0.9);
    }

    #[test]
    fn spearman_resists_a_single_outlier() {
        let x = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let y = [1.0, 2.0, 3.0, 4.0, 5.0, 1_000_000.0];
        assert!((spearman(&x, &y).unwrap() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn non_finite_pairs_are_dropped() {
        let x = [1.0, 2.0, f64::NAN, 4.0];
        let y = [1.0, 2.0, 3.0, 4.0];
        assert!((spearman(&x, &y).unwrap() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn summary_stats() {
        let s = summarize(&[0.1, 0.1, 0.1, -0.1]);
        assert_eq!(s.n, 4);
        assert!((s.mean - 0.05).abs() < 1e-12);
        assert!((s.hit_rate - 0.75).abs() < 1e-12);
        assert!(s.t_stat > 0.0);
        assert_eq!(summarize(&[]).n, 0);
    }
}
