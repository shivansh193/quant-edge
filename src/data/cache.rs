use anyhow::{Context, Result};
use chrono::NaiveDate;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::source::{AssetInfo, FundamentalSnapshot, MarketCap, PriceBar};
use super::sec_facts::PitFact;
use crate::universe::historical_membership::MembershipSnapshot;
use super::types::{InsiderTrade, IndustryCorrelation, MacroDataPoint, NewsItem, RedditSnapshot};

/// One row from the strategy_runs leaderboard table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyRunRow {
    pub id:            i64,
    pub run_at:        String,
    pub strategy_name: String,
    pub strategy_spec: String,
    pub total_return:  f64,
    pub sharpe:        f64,
    pub max_drawdown:  f64,
    pub alpha:         f64,
    pub ic_mean:       f64,
    pub start_date:    String,
    pub end_date:      String,
}

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
                operating_cashflow      REAL,
                return_on_assets        REAL,
                gross_profit_margin     REAL,
                fetched_at              TEXT NOT NULL,
                PRIMARY KEY (ticker, as_of_date)
            );

            -- Point-in-time company facts (SEC XBRL). Every row carries the date it
            -- was FILED; readers must only ever see rows with filed <= as_of.
            CREATE TABLE IF NOT EXISTS pit_facts (
                ticker       TEXT NOT NULL,
                concept      TEXT NOT NULL,   -- logical name, e.g. revenue, net_income
                period_start TEXT NOT NULL,   -- empty string for instant (balance sheet) facts
                period_end   TEXT NOT NULL,
                value        REAL NOT NULL,
                filed        TEXT NOT NULL,
                form         TEXT NOT NULL,
                fetched_at   TEXT NOT NULL,
                PRIMARY KEY (ticker, concept, period_start, period_end, filed)
            );
            CREATE INDEX IF NOT EXISTS idx_pit_facts_lookup ON pit_facts (ticker, filed);

            -- One row per S&P 500 membership CHANGE event (not one row per
            -- day): tickers is the full member list effective from date
            -- until the next row. See universe::historical_membership.
            CREATE TABLE IF NOT EXISTS sp500_membership (
                date       TEXT NOT NULL PRIMARY KEY,
                tickers    TEXT NOT NULL,
                fetched_at TEXT NOT NULL
            );

            -- Decision journal: your own discretionary calls, logged with a
            -- thesis BEFORE the outcome is known, closed and scored later.
            -- See journal.rs.
            CREATE TABLE IF NOT EXISTS decisions (
                id                     INTEGER PRIMARY KEY AUTOINCREMENT,
                ticker                 TEXT    NOT NULL,
                entry_date             TEXT    NOT NULL,
                thesis                 TEXT    NOT NULL,
                expected_holding_days  INTEGER NOT NULL,
                entry_price            REAL    NOT NULL,
                model_composite_at_entry REAL,
                status                 TEXT    NOT NULL DEFAULT 'open',  -- 'open' | 'closed'
                exit_date              TEXT,
                exit_price             REAL,
                outcome_notes          TEXT,
                created_at             TEXT    NOT NULL
            );

            CREATE TABLE IF NOT EXISTS universe_cache (
                market     TEXT NOT NULL,
                tickers    TEXT NOT NULL,
                fetched_at TEXT NOT NULL,
                PRIMARY KEY (market)
            );

            CREATE TABLE IF NOT EXISTS strategy_runs (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                run_at        TEXT    NOT NULL,
                strategy_name TEXT    NOT NULL,
                strategy_spec TEXT    NOT NULL,
                total_return  REAL    NOT NULL,
                sharpe        REAL    NOT NULL,
                max_drawdown  REAL    NOT NULL,
                alpha         REAL    NOT NULL DEFAULT 0,
                ic_mean       REAL    NOT NULL DEFAULT 0,
                start_date    TEXT    NOT NULL,
                end_date      TEXT    NOT NULL
            );
            ",
        )
        .context("Schema migration failed")?;

        // Additive columns for existing databases — errors mean column already exists
        for sql in &[
            "ALTER TABLE fundamentals ADD COLUMN operating_cashflow  REAL",
            "ALTER TABLE fundamentals ADD COLUMN return_on_assets    REAL",
            "ALTER TABLE fundamentals ADD COLUMN gross_profit_margin REAL",
            // Provenance. Rows written before this column existed are NULL and are
            // NOT trusted for historical dates: they hold a *current* snapshot
            // stamped with an old as_of date (look-ahead bias).
            "ALTER TABLE fundamentals ADD COLUMN source TEXT",
        ] {
            let _ = conn.execute(sql, []);
        }

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

    /// Cached fundamentals for exactly (ticker, as_of).
    ///
    /// When `require_point_in_time` is true only rows built from dated SEC
    /// filings (source = sec_pit) are returned. Use that for any historical
    /// date: legacy/Yahoo rows are current-day snapshots and would leak the
    /// future into a backtest.
    pub fn get_fundamentals(
        &self,
        ticker: &str,
        as_of: NaiveDate,
        require_point_in_time: bool,
    ) -> Result<Option<FundamentalSnapshot>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT revenue_ttm, revenue_cagr_3yr, net_margin_pct,
                    debt_to_equity, price_to_book, price_return_12m_1m, market_share_proxy,
                    operating_cashflow, return_on_assets, gross_profit_margin
             FROM fundamentals
             WHERE ticker = ?1 AND as_of_date = ?2
               AND (?3 = 0 OR source = 'sec_pit')",
        )?;

        let mut rows = stmt.query(params![ticker, as_of.to_string(), require_point_in_time as i64])?;
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
                operating_cashflow: row.get(7)?,
                return_on_assets: row.get(8)?,
                gross_profit_margin: row.get(9)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// Store a fundamentals snapshot. `source` records provenance:
    /// sec_pit (dated filings, safe for backtests) or yahoo_current (a live
    /// snapshot, only valid for today).
    pub fn insert_fundamentals(&self, snap: &FundamentalSnapshot, source: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO fundamentals
             (ticker, as_of_date, revenue_ttm, revenue_cagr_3yr, net_margin_pct,
              debt_to_equity, price_to_book, price_return_12m_1m, market_share_proxy,
              operating_cashflow, return_on_assets, gross_profit_margin, fetched_at, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, datetime('now'), ?13)",
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
                snap.operating_cashflow,
                snap.return_on_assets,
                snap.gross_profit_margin,
                source,
            ],
        )?;
        Ok(())
    }

    // ── Point-in-time SEC facts ───────────────────────────────────────────────

    pub fn insert_pit_facts(&self, ticker: &str, facts: &[PitFact]) -> Result<usize> {
        if facts.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let mut inserted = 0usize;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO pit_facts
                 (ticker, concept, period_start, period_end, value, filed, form, fetched_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'))",
            )?;
            for f in facts {
                inserted += stmt.execute(params![
                    ticker,
                    f.concept,
                    f.start.map(|d| d.to_string()).unwrap_or_default(),
                    f.end.to_string(),
                    f.value,
                    f.filed.to_string(),
                    f.form,
                ])?;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Every fact FILED on or before as_of. The filed <= as_of predicate is
    /// the point-in-time guarantee: a value that was not yet public cannot
    /// be returned, however it was later restated.
    pub fn get_pit_facts(&self, ticker: &str, as_of: NaiveDate) -> Result<Vec<PitFact>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT concept, period_start, period_end, value, filed, form
             FROM pit_facts
             WHERE ticker = ?1 AND filed <= ?2",
        )?;
        let rows = stmt
            .query_map(params![ticker, as_of.to_string()], |row| {
                let start: String = row.get(1)?;
                let end: String = row.get(2)?;
                let filed: String = row.get(4)?;
                Ok((row.get::<_, String>(0)?, start, end, row.get::<_, f64>(3)?, filed, row.get::<_, String>(5)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows
            .into_iter()
            .filter_map(|(concept, start, end, value, filed, form)| {
                Some(PitFact {
                    concept,
                    start: if start.is_empty() { None } else { start.parse().ok() },
                    end: end.parse().ok()?,
                    value,
                    filed: filed.parse().ok()?,
                    form,
                })
            })
            .collect())
    }

    /// True if SEC facts for this ticker were downloaded within max_age_days.
    pub fn has_pit_facts(&self, ticker: &str, max_age_days: i64) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM pit_facts
             WHERE ticker = ?1 AND fetched_at >= datetime('now', ?2)",
            params![ticker, format!("-{} days", max_age_days)],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            > 0
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
               AND filing_date <= ?3
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
             WHERE ticker = ?1 AND fetch_date >= ?2 AND fetch_date <= ?3
               -- A snapshot is only valid on the day it was taken. Rows whose
               -- fetched_at is well after their fetch_date were labelled with an
               -- old date by a historical run (current data, past label): hide them.
               AND date(fetched_at) <= date(fetch_date, '+3 days')",
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
        self.insert_reddit_snapshot_at(snap, None)
    }

    /// Like insert_reddit_snapshot, but records an explicit collection time
    /// (`YYYY-MM-DD HH:MM:SS`, UTC). `None` means "now". Exists so tests and
    /// imports can state when data was really collected: a snapshot is only
    /// valid for the date it was taken (see get_reddit_snapshots).
    pub fn insert_reddit_snapshot_at(&self, snap: &RedditSnapshot, fetched_at: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO reddit_mentions
             (ticker, fetch_date, subreddit, mention_count,
              avg_upvote_ratio, total_comments, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, COALESCE(?7, datetime('now')))",
            params![
                snap.ticker,
                snap.fetch_date.to_string(),
                snap.subreddit,
                snap.mention_count as i64,
                snap.avg_upvote_ratio,
                snap.total_comments as i64,
                fetched_at,
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

    /// The most recent correlation matrix computed on or before as_of and no
    /// older than max_age_days at as_of. Unlike get_industry_correlations this
    /// can never return a matrix computed from data after as_of, and it does not
    /// depend on the wall clock (the old freshness check made re-running an old
    /// backtest load a matrix built from the future).
    pub fn get_industry_correlations_asof(
        &self,
        window_days: u32,
        as_of: NaiveDate,
        max_age_days: i64,
    ) -> Result<Vec<IndustryCorrelation>> {
        let conn = self.conn.lock().unwrap();
        let oldest = as_of - chrono::Duration::days(max_age_days);
        let mut stmt = conn.prepare_cached(
            "SELECT industry_a, industry_b, correlation, date
             FROM industry_correlations
             WHERE window_days = ?1
               AND date = (
                   SELECT MAX(date) FROM industry_correlations
                   WHERE window_days = ?1 AND date <= ?2 AND date >= ?3
               )",
        )?;
        let rows = stmt
            .query_map(params![window_days, as_of.to_string(), oldest.to_string()], |row| {
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

    // ── Decision journal ─────────────────────────────────────────────────────

    pub fn insert_decision(&self, d: &crate::journal::NewDecision) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO decisions
             (ticker, entry_date, thesis, expected_holding_days, entry_price,
              model_composite_at_entry, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'open', datetime('now'))",
            params![
                d.ticker, d.entry_date.to_string(), d.thesis, d.expected_holding_days,
                d.entry_price, d.model_composite_at_entry,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn close_decision(
        &self,
        id: i64,
        exit_date: NaiveDate,
        exit_price: f64,
        outcome_notes: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE decisions SET status = 'closed', exit_date = ?1, exit_price = ?2, outcome_notes = ?3
             WHERE id = ?4 AND status = 'open'",
            params![exit_date.to_string(), exit_price, outcome_notes, id],
        )?;
        anyhow::ensure!(n == 1, "no open decision with id {id}");
        Ok(())
    }

    pub fn list_decisions(&self, status: Option<&str>) -> Result<Vec<crate::journal::JournalEntry>> {
        let conn = self.conn.lock().unwrap();
        let sql = "SELECT id, ticker, entry_date, thesis, expected_holding_days, entry_price,
                          model_composite_at_entry, status, exit_date, exit_price, outcome_notes
                   FROM decisions
                   WHERE (?1 IS NULL OR status = ?1)
                   ORDER BY entry_date ASC, id ASC";
        let mut stmt = conn.prepare_cached(sql)?;
        let rows = stmt
            .query_map(params![status], |r| {
                Ok(crate::journal::JournalEntry {
                    id: r.get(0)?,
                    ticker: r.get(1)?,
                    entry_date: r.get::<_, String>(2)?.parse().unwrap_or_default(),
                    thesis: r.get(3)?,
                    expected_holding_days: r.get(4)?,
                    entry_price: r.get(5)?,
                    model_composite_at_entry: r.get(6)?,
                    status: r.get(7)?,
                    exit_date: r.get::<_, Option<String>>(8)?.and_then(|s| s.parse().ok()),
                    exit_price: r.get(9)?,
                    outcome_notes: r.get(10)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ── S&P 500 point-in-time membership ──────────────────────────────────────

    pub fn has_sp500_membership_cache(&self, max_age_days: i64) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM sp500_membership WHERE fetched_at >= datetime('now', ?1)",
            params![format!("-{} days", max_age_days)],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            > 0
    }

    pub fn save_sp500_membership(&self, snapshots: &[MembershipSnapshot]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO sp500_membership (date, tickers, fetched_at)
                 VALUES (?1, ?2, datetime('now'))",
            )?;
            for s in snapshots {
                stmt.execute(params![s.date.to_string(), s.tickers.join(",")])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The membership row effective on `as_of`: the latest change-date <= as_of,
    /// or (if as_of precedes the dataset entirely) the earliest row on file.
    pub fn sp500_membership_as_of(&self, as_of: NaiveDate) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let row: Option<String> = conn
            .query_row(
                "SELECT tickers FROM sp500_membership WHERE date <= ?1 ORDER BY date DESC LIMIT 1",
                params![as_of.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        let row = match row {
            Some(t) => t,
            None => conn
                .query_row(
                    "SELECT tickers FROM sp500_membership ORDER BY date ASC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or_default(),
        };
        Ok(row.split(',').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect())
    }

    /// Union of every ticker that was a member at any change-event whose
    /// effective range overlaps [from, to] (the row just before `from` may
    /// already have been in effect at `from`, so it is included too).
    pub fn sp500_membership_union(&self, from: NaiveDate, to: NaiveDate) -> Result<std::collections::HashSet<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT tickers FROM sp500_membership
             WHERE date <= ?2 AND date >= (
                 SELECT COALESCE(MAX(date), '0000-01-01') FROM sp500_membership WHERE date <= ?1
             )",
        )?;
        let rows: Vec<String> = stmt
            .query_map(params![from.to_string(), to.to_string()], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut set = std::collections::HashSet::new();
        for row in rows {
            set.extend(row.split(',').filter(|s| !s.is_empty()).map(|s| s.to_string()));
        }
        Ok(set)
    }

    // ── Strategy runs (leaderboard) ───────────────────────────────────────────

    /// Persist one completed backtest result for the leaderboard.
    pub fn save_strategy_run(
        &self,
        strategy_name: &str,
        strategy_spec_json: &str,
        total_return: f64,
        sharpe: f64,
        max_drawdown: f64,
        alpha: f64,
        ic_mean: f64,
        start_date: &str,
        end_date: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO strategy_runs
             (run_at, strategy_name, strategy_spec, total_return, sharpe,
              max_drawdown, alpha, ic_mean, start_date, end_date)
             VALUES (datetime('now'), ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                strategy_name, strategy_spec_json,
                total_return, sharpe, max_drawdown, alpha, ic_mean,
                start_date, end_date,
            ],
        )?;
        Ok(())
    }

    /// Return all past strategy runs ordered by Sharpe ratio descending.
    pub fn load_strategy_history(&self) -> Result<Vec<StrategyRunRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT id, run_at, strategy_name, strategy_spec,
                    total_return, sharpe, max_drawdown, alpha, ic_mean,
                    start_date, end_date
             FROM strategy_runs
             ORDER BY sharpe DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(StrategyRunRow {
                id:            row.get(0)?,
                run_at:        row.get(1)?,
                strategy_name: row.get(2)?,
                strategy_spec: row.get(3)?,
                total_return:  row.get(4)?,
                sharpe:        row.get(5)?,
                max_drawdown:  row.get(6)?,
                alpha:         row.get(7)?,
                ic_mean:       row.get(8)?,
                start_date:    row.get(9)?,
                end_date:      row.get(10)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
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
#[cfg(test)]
mod sp500_membership_tests {
    use super::*;
    use crate::universe::historical_membership::MembershipSnapshot;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn snap(date: &str, tickers: &[&str]) -> MembershipSnapshot {
        MembershipSnapshot { date: d(date), tickers: tickers.iter().map(|s| s.to_string()).collect() }
    }

    fn seeded() -> Cache {
        let c = Cache::open(":memory:").unwrap();
        c.save_sp500_membership(&[
            snap("2020-01-01", &["AAPL", "MSFT", "GME"]),
            snap("2020-12-21", &["AAPL", "MSFT", "TSLA"]), // TSLA replaces GME
            snap("2021-06-01", &["AAPL", "MSFT", "TSLA", "COIN"]),
        ])
        .unwrap();
        c
    }

    #[test]
    fn as_of_picks_the_latest_change_on_or_before_the_date() {
        let c = seeded();
        assert_eq!(c.sp500_membership_as_of(d("2020-06-15")).unwrap(), vec!["AAPL", "MSFT", "GME"]);
        assert_eq!(c.sp500_membership_as_of(d("2020-12-21")).unwrap(), vec!["AAPL", "MSFT", "TSLA"]);
        assert_eq!(c.sp500_membership_as_of(d("2021-01-15")).unwrap(), vec!["AAPL", "MSFT", "TSLA"]);
        assert_eq!(c.sp500_membership_as_of(d("2021-06-01")).unwrap(), vec!["AAPL", "MSFT", "TSLA", "COIN"]);
    }

    #[test]
    fn as_of_before_the_dataset_falls_back_to_the_earliest_row() {
        let c = seeded();
        assert_eq!(c.sp500_membership_as_of(d("1999-01-01")).unwrap(), vec!["AAPL", "MSFT", "GME"]);
    }

    #[test]
    fn union_covers_every_member_across_the_window_including_the_row_just_before_it() {
        let c = seeded();
        // Window entirely within the GME era: union == that one snapshot.
        let u = c.sp500_membership_union(d("2020-02-01"), d("2020-03-01")).unwrap();
        let mut v: Vec<_> = u.into_iter().collect();
        v.sort();
        assert_eq!(v, vec!["AAPL", "GME", "MSFT"]);

        // Window straddling the GME->TSLA swap: union has both.
        let u = c.sp500_membership_union(d("2020-12-01"), d("2020-12-31")).unwrap();
        assert!(u.contains("GME") && u.contains("TSLA"), "{u:?}");
        assert!(!u.contains("COIN"));
    }

    #[test]
    fn has_cache_respects_the_max_age_window() {
        let c = seeded();
        assert!(c.has_sp500_membership_cache(30));
        assert!(!c.has_sp500_membership_cache(-1), "a negative window must never be considered fresh");
    }

    #[test]
    fn re_saving_overwrites_rather_than_duplicating() {
        let c = seeded();
        c.save_sp500_membership(&[snap("2020-01-01", &["ONLY"])]).unwrap();
        assert_eq!(c.sp500_membership_as_of(d("2020-06-01")).unwrap(), vec!["ONLY"]);
    }
}
