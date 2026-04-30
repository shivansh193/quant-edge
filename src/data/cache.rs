use anyhow::{Context, Result};
use chrono::NaiveDate;
use rusqlite::{Connection, params};
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::source::{AssetInfo, FundamentalSnapshot, MarketCap, PriceBar};
use super::types::{InsiderTrade, IndustryCorrelation, MacroDataPoint, NewsItem, RedditSnapshot};

/// Thread-safe SQLite cache.
/// Wraps connection in Arc<Mutex<>> — fine for our single-process use case.
#[derive(Clone)]
pub struct Cache {
    conn: Arc<Mutex<Connection>>,
}

impl Cache {
    /// Open (or create) the SQLite DB at `path` and run migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path).context("Failed to open SQLite cache")?;
        let cache = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        cache.migrate()?;
        Ok(cache)
    }

    /// Run schema migrations — idempotent, safe to call on every startup.
    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;

            CREATE TABLE IF NOT EXISTS insider_trades (
                id               INTEGER PRIMARY KEY AUTOINCREMENT,
                ticker           TEXT    NOT NULL,
                filing_date      TEXT    NOT NULL,
                trade_date       TEXT    NOT NULL,
                insider_name     TEXT    NOT NULL,
                insider_role     TEXT    NOT NULL,
                shares           REAL    NOT NULL,
                transaction_type TEXT    NOT NULL,
                fetched_at       TEXT    NOT NULL,
                UNIQUE(ticker, filing_date, insider_name, trade_date, shares)
            );

            CREATE TABLE IF NOT EXISTS news_sentiment (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                ticker       TEXT    NOT NULL,
                article_date TEXT    NOT NULL,
                tone         REAL    NOT NULL,
                headline     TEXT    NOT NULL DEFAULT '',
                source       TEXT    NOT NULL DEFAULT '',
                fetched_at   TEXT    NOT NULL
            );

            CREATE TABLE IF NOT EXISTS reddit_mentions (
                ticker            TEXT NOT NULL,
                fetch_date        TEXT NOT NULL,
                subreddit         TEXT NOT NULL,
                mention_count     INTEGER NOT NULL,
                avg_upvote_ratio  REAL    NOT NULL,
                total_comments    INTEGER NOT NULL,
                fetched_at        TEXT    NOT NULL,
                PRIMARY KEY (ticker, fetch_date, subreddit)
            );

            CREATE TABLE IF NOT EXISTS macro_data (
                series_id  TEXT NOT NULL,
                date       TEXT NOT NULL,
                value      REAL NOT NULL,
                fetched_at TEXT NOT NULL,
                PRIMARY KEY (series_id, date)
            );

            CREATE TABLE IF NOT EXISTS price_bars (
                ticker      TEXT NOT NULL,
                date        TEXT NOT NULL,   -- ISO 8601: YYYY-MM-DD
                open        REAL NOT NULL,
                high        REAL NOT NULL,
                low         REAL NOT NULL,
                close       REAL NOT NULL,
                adj_close   REAL NOT NULL,
                volume      INTEGER NOT NULL,
                PRIMARY KEY (ticker, date)
            );

            CREATE TABLE IF NOT EXISTS asset_info (
                ticker          TEXT PRIMARY KEY,
                name            TEXT,
                exchange        TEXT,
                currency        TEXT,
                gics_industry   TEXT,
                market_cap_usd  REAL,
                market_cap_tier TEXT,
                fetched_at      TEXT NOT NULL   -- ISO 8601 datetime
            );

            CREATE TABLE IF NOT EXISTS industry_correlations (
                industry_a   TEXT    NOT NULL,
                industry_b   TEXT    NOT NULL,
                correlation  REAL    NOT NULL,
                date         TEXT    NOT NULL,
                window_days  INTEGER NOT NULL,
                fetched_at   TEXT    NOT NULL,
                PRIMARY KEY (industry_a, industry_b, date, window_days)
            );

            CREATE TABLE IF NOT EXISTS fundamentals (
                ticker                  TEXT NOT NULL,
                as_of_date              TEXT NOT NULL,   -- rebalancing date
                revenue_ttm             REAL,
                revenue_cagr_3yr        REAL,
                net_margin_pct          REAL,
                debt_to_equity          REAL,
                price_to_book           REAL,
                price_return_12m_1m     REAL,
                market_share_proxy      REAL,
                fetched_at              TEXT NOT NULL,
                PRIMARY KEY (ticker, as_of_date)
            );

            CREATE TABLE IF NOT EXISTS universe_cache (
                market     TEXT NOT NULL,
                tickers    TEXT NOT NULL,
                fetched_at TEXT NOT NULL,
                PRIMARY KEY (market)
            );
            ",
        )
        .context("Schema migration failed")?;
        Ok(())
    }

    // ── Price bars ────────────────────────────────────────────────────────────

    pub fn get_price_bars(
        &self,
        ticker: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<PriceBar>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT date, open, high, low, close, adj_close, volume
             FROM price_bars
             WHERE ticker = ?1 AND date >= ?2 AND date <= ?3
             ORDER BY date ASC",
        )?;

        let bars = stmt
            .query_map(
                params![ticker, from.to_string(), to.to_string()],
                |row| {
                    Ok(PriceBar {
                        date: row.get::<_, String>(0)?.parse().unwrap(),
                        open: row.get(1)?,
                        high: row.get(2)?,
                        low: row.get(3)?,
                        close: row.get(4)?,
                        adj_close: row.get(5)?,
                        volume: row.get(6)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(bars)
    }

    pub fn insert_price_bars(&self, ticker: &str, bars: &[PriceBar]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO price_bars
                 (ticker, date, open, high, low, close, adj_close, volume)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;

            for bar in bars {
                stmt.execute(params![
                    ticker,
                    bar.date.to_string(),
                    bar.open,
                    bar.high,
                    bar.low,
                    bar.close,
                    bar.adj_close,
                    bar.volume,
                ])?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// Returns true if we have a full coverage of bars for the range.
    /// Heuristic: cached bar count ≥ 90% of expected trading days.
    pub fn has_price_coverage(&self, ticker: &str, from: NaiveDate, to: NaiveDate) -> bool {
        let Ok(bars) = self.get_price_bars(ticker, from, to) else {
            return false;
        };
        let calendar_days = (to - from).num_days();
        let expected_trading_days = (calendar_days as f64 * 5.0 / 7.0) as usize;
        bars.len() >= (expected_trading_days as f64 * 0.9) as usize
    }

    // ── Asset info ────────────────────────────────────────────────────────────

    pub fn get_asset_info(&self, ticker: &str) -> Result<Option<AssetInfo>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT ticker, name, exchange, currency, gics_industry,
                    market_cap_usd, market_cap_tier
             FROM asset_info WHERE ticker = ?1",
        )?;

        let mut rows = stmt.query(params![ticker])?;
        if let Some(row) = rows.next()? {
            Ok(Some(AssetInfo {
                ticker: row.get(0)?,
                name: row.get(1)?,
                exchange: row.get(2)?,
                currency: row.get(3)?,
                gics_industry: row.get(4)?,
                market_cap_usd: row.get(5)?,
                market_cap_tier: row
                    .get::<_, Option<String>>(6)?
                    .map(|s| match s.as_str() {
                        "SmallCap" => MarketCap::SmallCap,
                        "MidCap" => MarketCap::MidCap,
                        _ => MarketCap::LargeCap,
                    }),
            }))
        } else {
            Ok(None)
        }
    }

    pub fn insert_asset_info(&self, info: &AssetInfo) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO asset_info
             (ticker, name, exchange, currency, gics_industry,
              market_cap_usd, market_cap_tier, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'))",
            params![
                info.ticker,
                info.name,
                info.exchange,
                info.currency,
                info.gics_industry,
                info.market_cap_usd,
                info.market_cap_tier.as_ref().map(|t| format!("{:?}", t)),
            ],
        )?;
        Ok(())
    }

    // ── Fundamentals ──────────────────────────────────────────────────────────

    pub fn get_fundamentals(
        &self,
        ticker: &str,
        as_of: NaiveDate,
    ) -> Result<Option<FundamentalSnapshot>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT revenue_ttm, revenue_cagr_3yr, net_margin_pct,
                    debt_to_equity, price_to_book, price_return_12m_1m, market_share_proxy
             FROM fundamentals
             WHERE ticker = ?1 AND as_of_date = ?2",
        )?;

        let mut rows = stmt.query(params![ticker, as_of.to_string()])?;
        if let Some(row) = rows.next()? {
            Ok(Some(FundamentalSnapshot {
                ticker: ticker.to_string(),
                date: as_of,
                revenue_ttm: row.get(0)?,
                revenue_cagr_3yr: row.get(1)?,
                net_margin_pct: row.get(2)?,
                debt_to_equity: row.get(3)?,
                price_to_book: row.get(4)?,
                price_return_12m_1m: row.get(5)?,
                market_share_proxy: row.get(6)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn insert_fundamentals(&self, snap: &FundamentalSnapshot) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO fundamentals
             (ticker, as_of_date, revenue_ttm, revenue_cagr_3yr, net_margin_pct,
              debt_to_equity, price_to_book, price_return_12m_1m, market_share_proxy,
              fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, datetime('now'))",
            params![
                snap.ticker,
                snap.date.to_string(),
                snap.revenue_ttm,
                snap.revenue_cagr_3yr,
                snap.net_margin_pct,
                snap.debt_to_equity,
                snap.price_to_book,
                snap.price_return_12m_1m,
                snap.market_share_proxy,
            ],
        )?;
        Ok(())
    }

    // ── Insider trades ────────────────────────────────────────────────────────

    pub fn get_insider_trades(
        &self,
        ticker: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<InsiderTrade>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT filing_date, trade_date, insider_name, insider_role,
                    shares, transaction_type
             FROM insider_trades
             WHERE ticker = ?1 AND trade_date >= ?2 AND trade_date <= ?3
             ORDER BY trade_date DESC",
        )?;

        let rows = stmt.query_map(
            params![ticker, from.to_string(), to.to_string()],
            |row| {
                Ok(InsiderTrade {
                    ticker: ticker.to_string(),
                    filing_date: row.get::<_, String>(0)?.parse().unwrap_or(from),
                    trade_date: row.get::<_, String>(1)?.parse().unwrap_or(from),
                    insider_name: row.get(2)?,
                    insider_role: row.get(3)?,
                    shares: row.get(4)?,
                    transaction_type: row.get(5)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    pub fn insert_insider_trades(&self, trades: &[InsiderTrade]) -> Result<()> {
        if trades.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO insider_trades
                 (ticker, filing_date, trade_date, insider_name, insider_role,
                  shares, transaction_type, fetched_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'))",
            )?;
            for t in trades {
                stmt.execute(params![
                    t.ticker,
                    t.filing_date.to_string(),
                    t.trade_date.to_string(),
                    t.insider_name,
                    t.insider_role,
                    t.shares,
                    t.transaction_type,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// True if we have insider data for this ticker fetched within `hours` hours.
    pub fn has_insider_cache(&self, ticker: &str, hours: i64) -> bool {
        let conn = self.conn.lock().unwrap();
        let cutoff = format!("-{} hours", hours);
        conn.query_row(
            "SELECT COUNT(*) FROM insider_trades
             WHERE ticker = ?1 AND fetched_at >= datetime('now', ?2)",
            params![ticker, cutoff],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            > 0
    }

    // ── News sentiment ────────────────────────────────────────────────────────

    pub fn get_news_items(
        &self,
        ticker: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<NewsItem>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT article_date, tone, headline, source
             FROM news_sentiment
             WHERE ticker = ?1 AND article_date >= ?2 AND article_date <= ?3
             ORDER BY article_date DESC",
        )?;

        let rows = stmt.query_map(
            params![ticker, from.to_string(), to.to_string()],
            |row| {
                Ok(NewsItem {
                    ticker: ticker.to_string(),
                    article_date: row.get::<_, String>(0)?.parse().unwrap_or(from),
                    tone: row.get(1)?,
                    headline: row.get(2)?,
                    source: row.get(3)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    pub fn insert_news_items(&self, items: &[NewsItem]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO news_sentiment
                 (ticker, article_date, tone, headline, source, fetched_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
            )?;
            for item in items {
                stmt.execute(params![
                    item.ticker,
                    item.article_date.to_string(),
                    item.tone,
                    item.headline,
                    item.source,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// True if news data for this ticker was fetched within `hours` hours.
    pub fn has_news_cache(&self, ticker: &str, hours: i64) -> bool {
        let conn = self.conn.lock().unwrap();
        let cutoff = format!("-{} hours", hours);
        conn.query_row(
            "SELECT COUNT(*) FROM news_sentiment
             WHERE ticker = ?1 AND fetched_at >= datetime('now', ?2)",
            params![ticker, cutoff],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            > 0
    }

    // ── Reddit mentions ───────────────────────────────────────────────────────

    pub fn get_reddit_snapshots(
        &self,
        ticker: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<RedditSnapshot>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT fetch_date, subreddit, mention_count,
                    avg_upvote_ratio, total_comments
             FROM reddit_mentions
             WHERE ticker = ?1 AND fetch_date >= ?2 AND fetch_date <= ?3",
        )?;

        let rows = stmt.query_map(
            params![ticker, from.to_string(), to.to_string()],
            |row| {
                Ok(RedditSnapshot {
                    ticker: ticker.to_string(),
                    fetch_date: row.get::<_, String>(0)?.parse().unwrap_or(from),
                    subreddit: row.get(1)?,
                    mention_count: row.get::<_, i64>(2)? as u32,
                    avg_upvote_ratio: row.get(3)?,
                    total_comments: row.get::<_, i64>(4)? as u32,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    pub fn insert_reddit_snapshot(&self, snap: &RedditSnapshot) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO reddit_mentions
             (ticker, fetch_date, subreddit, mention_count,
              avg_upvote_ratio, total_comments, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))",
            params![
                snap.ticker,
                snap.fetch_date.to_string(),
                snap.subreddit,
                snap.mention_count as i64,
                snap.avg_upvote_ratio,
                snap.total_comments as i64,
            ],
        )?;
        Ok(())
    }

    /// True if Reddit data for this ticker was fetched within `hours` hours.
    pub fn has_reddit_cache(&self, ticker: &str, hours: i64) -> bool {
        let conn = self.conn.lock().unwrap();
        let cutoff = format!("-{} hours", hours);
        conn.query_row(
            "SELECT COUNT(*) FROM reddit_mentions
             WHERE ticker = ?1 AND fetched_at >= datetime('now', ?2)",
            params![ticker, cutoff],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            > 0
    }

    // ── Macro data (FRED) ─────────────────────────────────────────────────────

    pub fn get_macro_data(
        &self,
        series_id: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<MacroDataPoint>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT date, value FROM macro_data
             WHERE series_id = ?1 AND date >= ?2 AND date <= ?3
             ORDER BY date ASC",
        )?;

        let rows = stmt.query_map(
            params![series_id, from.to_string(), to.to_string()],
            |row| {
                Ok(MacroDataPoint {
                    series_id: series_id.to_string(),
                    date: row.get::<_, String>(0)?.parse().unwrap_or(from),
                    value: row.get(1)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    pub fn insert_macro_data(&self, points: &[MacroDataPoint]) -> Result<()> {
        if points.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO macro_data
                 (series_id, date, value, fetched_at)
                 VALUES (?1, ?2, ?3, datetime('now'))",
            )?;
            for p in points {
                stmt.execute(params![p.series_id, p.date.to_string(), p.value])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ── Industry correlations ─────────────────────────────────────────────────

    /// Returns true if correlation data was computed within the last 7 days.
    pub fn has_correlation_cache(&self, window_days: u32) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM industry_correlations
             WHERE window_days = ?1 AND fetched_at >= datetime('now', '-168 hours')",
            params![window_days],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            > 0
    }

    /// Returns the most recently stored correlation rows for the given window.
    pub fn get_industry_correlations(&self, window_days: u32) -> Result<Vec<IndustryCorrelation>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT industry_a, industry_b, correlation, date
             FROM industry_correlations
             WHERE date = (
                 SELECT MAX(date) FROM industry_correlations WHERE window_days = ?1
             )
             AND window_days = ?1",
        )?;

        let rows = stmt.query_map(params![window_days], |row| {
            Ok(IndustryCorrelation {
                industry_a: row.get(0)?,
                industry_b: row.get(1)?,
                correlation: row.get(2)?,
                date: row
                    .get::<_, String>(3)?
                    .parse()
                    .unwrap_or_else(|_| chrono::NaiveDate::from_ymd_opt(2000, 1, 1).unwrap()),
                window_days,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    pub fn insert_industry_correlations(&self, corrs: &[IndustryCorrelation]) -> Result<()> {
        if corrs.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO industry_correlations
                 (industry_a, industry_b, correlation, date, window_days, fetched_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
            )?;
            for c in corrs {
                stmt.execute(params![
                    c.industry_a,
                    c.industry_b,
                    c.correlation,
                    c.date.to_string(),
                    c.window_days,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// True if macro series has data fetched within `hours` hours.
    pub fn has_macro_cache(&self, series_id: &str, hours: i64) -> bool {
        let conn = self.conn.lock().unwrap();
        let cutoff = format!("-{} hours", hours);
        conn.query_row(
            "SELECT COUNT(*) FROM macro_data
             WHERE series_id = ?1 AND fetched_at >= datetime('now', ?2)",
            params![series_id, cutoff],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            > 0
    }

    // ── Paper portfolio persistence ───────────────────────────────────────────

    /// Ensure the paper_portfolio table exists and upsert the JSON blob.
    pub fn save_paper_portfolio(&self, json: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS paper_portfolio (
                id         INTEGER PRIMARY KEY,
                data       TEXT    NOT NULL,
                updated_at TEXT    NOT NULL
            );",
        )
        .context("Failed to create paper_portfolio table")?;

        conn.execute(
            "INSERT INTO paper_portfolio (id, data, updated_at)
             VALUES (1, ?1, datetime('now'))
             ON CONFLICT(id) DO UPDATE SET data = excluded.data, updated_at = excluded.updated_at",
            params![json],
        )
        .context("Failed to upsert paper portfolio")?;

        Ok(())
    }

    // ── Universe cache ────────────────────────────────────────────────────────

    pub fn save_universe_cache(&self, market: &str, tickers: &[String]) -> Result<()> {
        let json = serde_json::to_string(tickers).context("Failed to serialize ticker list")?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO universe_cache (market, tickers, fetched_at)
             VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(market) DO UPDATE SET tickers = excluded.tickers, fetched_at = excluded.fetched_at",
            params![market, json],
        )?;
        Ok(())
    }

    /// Returns cached tickers if data is newer than `ttl_days` days, else None.
    pub fn load_universe_cache(&self, market: &str, ttl_days: i64) -> Result<Option<Vec<String>>> {
        let cutoff = format!("-{} days", ttl_days);
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT tickers FROM universe_cache
             WHERE market = ?1 AND fetched_at >= datetime('now', ?2)",
            params![market, cutoff],
            |r| r.get::<_, String>(0),
        );
        match result {
            Ok(json) => {
                let tickers: Vec<String> = serde_json::from_str(&json)
                    .context("Failed to deserialize ticker list")?;
                Ok(Some(tickers))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Load the stored paper portfolio JSON blob, returning None if none exists yet.
    pub fn load_paper_portfolio(&self) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();

        // Table may not exist yet
        let exists: bool = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='paper_portfolio'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0) > 0;

        if !exists {
            return Ok(None);
        }

        let result = conn.query_row(
            "SELECT data FROM paper_portfolio WHERE id = 1",
            [],
            |r| r.get::<_, String>(0),
        );

        match result {
            Ok(json)                         => Ok(Some(json)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e)                           => Err(e.into()),
        }
    }
}