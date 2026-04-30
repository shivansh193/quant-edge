use super::{MarketData, Signal, clamp_signal, mean};

/// Sentiment signal combining:
///   - GDELT news tone (7-day delta: is sentiment improving or worsening?)
///   - Reddit buzz score (normalised mentions × upvote_ratio)
/// Weight: news 70%, reddit 30%
pub struct SentimentSignal;

impl Signal for SentimentSignal {
    fn name(&self) -> &str {
        "Sentiment"
    }

    fn compute(&self, _ticker: &str, data: &MarketData) -> f64 {
        let news_score   = compute_news_score(data);
        let reddit_score = compute_reddit_score(data);

        // 70% news, 30% reddit
        let raw = 0.70 * news_score + 0.30 * reddit_score;
        clamp_signal(raw)
    }
}

// ── GDELT news score ──────────────────────────────────────────────────────────

fn compute_news_score(data: &MarketData) -> f64 {
    let items = &data.news_items;
    if items.is_empty() {
        return 0.0;
    }

    let as_of = data.as_of;

    // Split into last-7-days and 8-30 days ago
    let recent: Vec<f64> = items
        .iter()
        .filter(|it| (as_of - it.article_date).num_days() <= 7)
        .map(|it| it.tone)
        .collect();

    let older: Vec<f64> = items
        .iter()
        .filter(|it| {
            let d = (as_of - it.article_date).num_days();
            d > 7 && d <= 30
        })
        .map(|it| it.tone)
        .collect();

    let recent_tone = mean(&recent);
    let older_tone  = mean(&older);

    // GDELT tone range is roughly -10 to +10 for news
    let level_score  = clamp_signal(recent_tone / 10.0);
    let delta_score  = if !older.is_empty() {
        clamp_signal((recent_tone - older_tone) / 5.0) // delta normalised
    } else {
        level_score
    };

    // 60% current level, 40% improving delta
    0.60 * level_score + 0.40 * delta_score
}

// ── Reddit buzz score ─────────────────────────────────────────────────────────

fn compute_reddit_score(data: &MarketData) -> f64 {
    let snaps = &data.reddit_snapshots;
    if snaps.is_empty() {
        return 0.0;
    }

    // Total buzz: sum of (mention_count × upvote_ratio) across subreddits
    let total_mentions: f64 = snaps.iter().map(|s| s.mention_count as f64).sum();
    let buzz_score: f64 = snaps
        .iter()
        .map(|s| s.mention_count as f64 * s.avg_upvote_ratio)
        .sum();

    if total_mentions <= 0.0 {
        return 0.0;
    }

    // Normalise: 50 mentions with 0.75 upvote ratio is roughly neutral
    // buzz = 50 × 0.75 = 37.5 → aim for that to map to ~0
    // Scale: buzz / 100 caps at +1 around 100 buzz score
    let normalised_buzz = (buzz_score / 100.0).min(1.0);

    // Upvote ratio quality signal: >0.6 = positive, <0.4 = negative
    let avg_ratio = snaps.iter().map(|s| s.avg_upvote_ratio).sum::<f64>() / snaps.len() as f64;
    let quality = clamp_signal((avg_ratio - 0.5) * 4.0); // ±0.5 from neutral

    // Combine: 50% buzz volume, 50% quality
    clamp_signal(0.50 * normalised_buzz + 0.50 * quality)
}
