use anyhow::Result;
use chrono::NaiveDate;
use ndarray::Array2;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, warn};

use crate::data::{cache::Cache, IndustryCorrelation};

pub const WINDOW_DAYS: u32 = 90;
const MIN_OVERLAP: usize = 60;

// ── Public data container ─────────────────────────────────────────────────────

/// Precomputed correlation data — passed to PairsSignal as Arc.
pub struct CorrelationData {
    /// (industry_a, industry_b) in sorted string order → Pearson r
    pub matrix: HashMap<(String, String), f64>,
    /// industry → date → equal-weighted average daily return
    pub industry_returns: HashMap<String, HashMap<NaiveDate, f64>>,
    pub as_of: NaiveDate,
}

impl CorrelationData {
    pub fn get_correlation(&self, a: &str, b: &str) -> Option<f64> {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        self.matrix.get(&(lo.to_string(), hi.to_string())).copied()
    }

    /// Up to `top_n` industries most correlated with `industry`, filtered by min_r.
    /// Returns (industry_name, correlation) sorted descending.
    pub fn top_correlated(&self, industry: &str, min_r: f64, top_n: usize) -> Vec<(String, f64)> {
        let mut peers: Vec<(String, f64)> = self
            .matrix
            .iter()
            .filter_map(|((a, b), &r)| {
                if r < min_r {
                    return None;
                }
                if a == industry {
                    Some((b.clone(), r))
                } else if b == industry {
                    Some((a.clone(), r))
                } else {
                    None
                }
            })
            .collect();
        peers.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap_or(std::cmp::Ordering::Equal));
        peers.truncate(top_n);
        peers
    }

    /// All pairs sorted by |correlation| descending.
    pub fn sorted_pairs(&self) -> Vec<(&(String, String), f64)> {
        let mut v: Vec<_> = self.matrix.iter().map(|(k, &r)| (k, r)).collect();
        v.sort_by(|a, b| b.1.abs().partial_cmp(&a.1.abs()).unwrap_or(std::cmp::Ordering::Equal));
        v
    }
}

// ── Engine ────────────────────────────────────────────────────────────────────

pub struct CorrelationEngine {
    cache: Cache,
}

impl CorrelationEngine {
    pub fn new(cache: Cache) -> Self {
        Self { cache }
    }

    /// Load from SQLite cache if fresh (< 7 days old), otherwise compute from price_bars.
    /// `industry_tickers`: industry name → list of ticker symbols.
    pub fn load_or_compute(
        &self,
        industry_tickers: &HashMap<String, Vec<String>>,
        as_of: NaiveDate,
    ) -> Result<Arc<CorrelationData>> {
        let from = as_of - chrono::Duration::days(WINDOW_DAYS as i64 + 15);

        let industry_returns = self.compute_industry_returns(industry_tickers, from, as_of)?;

        if self.cache.has_correlation_cache(WINDOW_DAYS) {
            if let Ok(stored) = self.cache.get_industry_correlations(WINDOW_DAYS) {
                if !stored.is_empty() {
                    let n = stored.len();
                    let matrix = stored
                        .into_iter()
                        .map(|ic| ((ic.industry_a, ic.industry_b), ic.correlation))
                        .collect();
                    info!("Loaded {} correlation pairs from cache (as_of={})", n, as_of);
                    return Ok(Arc::new(CorrelationData { matrix, industry_returns, as_of }));
                }
            }
        }

        let matrix = compute_correlation_matrix(&industry_returns)?;

        let to_insert: Vec<IndustryCorrelation> = matrix
            .iter()
            .map(|((a, b), &r)| IndustryCorrelation {
                industry_a: a.clone(),
                industry_b: b.clone(),
                correlation: r,
                date: as_of,
                window_days: WINDOW_DAYS,
            })
            .collect();

        if let Err(e) = self.cache.insert_industry_correlations(&to_insert) {
            warn!("Failed to persist correlation matrix: {e:#}");
        }

        info!(
            "Computed correlation matrix: {} industry pairs from {} industries",
            matrix.len(),
            industry_returns.len()
        );

        Ok(Arc::new(CorrelationData { matrix, industry_returns, as_of }))
    }

    fn compute_industry_returns(
        &self,
        industry_tickers: &HashMap<String, Vec<String>>,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<HashMap<String, HashMap<NaiveDate, f64>>> {
        let mut result: HashMap<String, HashMap<NaiveDate, f64>> = HashMap::new();

        for (industry, tickers) in industry_tickers {
            if tickers.is_empty() {
                continue;
            }

            // Accumulate per-date returns across all tickers
            let mut date_buckets: HashMap<NaiveDate, Vec<f64>> = HashMap::new();

            for ticker in tickers {
                let bars = self.cache.get_price_bars(ticker, from, to).unwrap_or_default();
                for i in 1..bars.len() {
                    let prev = bars[i - 1].adj_close;
                    let curr = bars[i].adj_close;
                    if prev > 1e-9 {
                        let ret = (curr - prev) / prev;
                        if ret.is_finite() {
                            date_buckets.entry(bars[i].date).or_default().push(ret);
                        }
                    }
                }
            }

            let by_date: HashMap<NaiveDate, f64> = date_buckets
                .into_iter()
                .map(|(d, vs)| (d, vs.iter().sum::<f64>() / vs.len() as f64))
                .collect();

            if !by_date.is_empty() {
                result.insert(industry.clone(), by_date);
            }
        }

        Ok(result)
    }
}

// ── Matrix computation ────────────────────────────────────────────────────────

fn compute_correlation_matrix(
    industry_returns: &HashMap<String, HashMap<NaiveDate, f64>>,
) -> Result<HashMap<(String, String), f64>> {
    let mut industries: Vec<&String> = industry_returns.keys().collect();
    industries.sort(); // deterministic ordering
    let n = industries.len();

    if n < 2 {
        return Ok(HashMap::new());
    }

    // Use Array2 to hold the symmetric correlation matrix during computation.
    let mut arr = Array2::<f64>::from_elem((n, n), f64::NAN);
    for i in 0..n {
        arr[[i, i]] = 1.0;
    }

    for i in 0..n {
        for j in (i + 1)..n {
            let a = industries[i];
            let b = industries[j];
            if let Some(r) = pearson_from_date_maps(&industry_returns[a], &industry_returns[b]) {
                arr[[i, j]] = r;
                arr[[j, i]] = r;
            }
        }
    }

    // Convert upper triangle of Array2 into HashMap
    let mut out = HashMap::new();
    for i in 0..n {
        for j in (i + 1)..n {
            let r = arr[[i, j]];
            if r.is_finite() {
                let a = industries[i].clone();
                let b = industries[j].clone();
                // Store with lexicographic key so lookups are consistent
                let key = if a <= b { (a, b) } else { (b, a) };
                out.insert(key, r);
            }
        }
    }

    Ok(out)
}

// ── Pearson correlation ───────────────────────────────────────────────────────

/// Compute Pearson r between two industry return series aligned by date.
/// Returns None when fewer than MIN_OVERLAP dates overlap.
fn pearson_from_date_maps(
    a: &HashMap<NaiveDate, f64>,
    b: &HashMap<NaiveDate, f64>,
) -> Option<f64> {
    let pairs: Vec<(f64, f64)> = a
        .iter()
        .filter_map(|(d, &va)| b.get(d).map(|&vb| (va, vb)))
        .filter(|(va, vb)| va.is_finite() && vb.is_finite())
        .collect();

    if pairs.len() < MIN_OVERLAP {
        return None;
    }

    let n = pairs.len() as f64;
    let mx = pairs.iter().map(|(x, _)| x).sum::<f64>() / n;
    let my = pairs.iter().map(|(_, y)| y).sum::<f64>() / n;

    let (mut num, mut dx2, mut dy2) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (x, y) in &pairs {
        let dx = x - mx;
        let dy = y - my;
        num += dx * dy;
        dx2 += dx * dx;
        dy2 += dy * dy;
    }

    let denom = (dx2 * dy2).sqrt();
    if denom < 1e-12 {
        return None;
    }

    Some((num / denom).clamp(-1.0, 1.0))
}
