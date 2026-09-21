// ── Shared primitives ─────────────────────────────────────────────────────────

export interface SignalWeights {
  momentum?: number | null
  fundamental?: number | null
  insider?: number | null
  sentiment?: number | null
  pairs?: number | null
}

export interface StrategyFilters {
  sectors?: string[] | null
  exclude_sectors?: string[] | null
  market_cap_tiers?: string[] | null
  min_market_cap_usd?: number | null
  min_daily_volume_usd?: number | null
}

export interface StrategySpec {
  name: string
  description: string
  signal_weights: SignalWeights
  filters: StrategyFilters
  top_n: number
  holding_period_days: number
  universe_override?: string[] | null
}

export interface PresetStrategy {
  id: string
  name: string
  description: string
  icon: string
  spec: StrategySpec
}

// ── Signal scores ─────────────────────────────────────────────────────────────

export interface SignalScore {
  rank: number
  ticker: string
  industry: string
  composite: number
  momentum_raw: number
  fundamental_raw: number
  insider_raw: number
  sentiment_raw: number
  pairs_raw: number
  momentum_contrib: number
  fundamental_contrib: number
  insider_contrib: number
  sentiment_contrib: number
  pairs_contrib: number
  macro_on: boolean
  /** Which signals had real input data. Missing signals are excluded from `composite`. */
  availability?: {
    momentum: boolean
    fundamental: boolean
    insider: boolean
    sentiment: boolean
    pairs: boolean
  }
}

export interface SignalDetailResponse {
  ticker: string
  as_of: string
  score: SignalScore
  fundamentals: FundamentalSnapshot | null
  news_count: number
  insider_trade_count: number
  reddit_mention_count: number
}

export interface FundamentalSnapshot {
  ticker: string
  date: string
  revenue_ttm: number | null
  revenue_cagr_3yr: number | null
  net_margin_pct: number | null
  debt_to_equity: number | null
  price_to_book: number | null
  price_return_12m_1m: number | null
  market_share_proxy: number | null
  operating_cashflow: number | null
  return_on_assets: number | null
  gross_profit_margin: number | null
}

// ── Morning / Evening ─────────────────────────────────────────────────────────

export interface MacroContext {
  vix: number | null
  yield_10y: number | null
  yield_2y: number | null
  regime: string
  nifty50_1d_pct: number | null
  sp500_1d_pct: number | null
  usdinr: number | null
}

export interface PickSummary {
  rank: number
  ticker: string
  industry: string
  score: number
  price_change_1d_pct: number | null
  macro_on: boolean
}

export interface PaperSnapshot {
  total_value: number
  pnl_pct: number
  cash_pct: number
  position_count: number
  last_rebalance: string | null
}

export interface MorningResponse {
  date: string
  macro: MacroContext
  top_picks: PickSummary[]
  paper_snapshot: PaperSnapshot | null
  universe_size: number
}

export interface EveningMover {
  ticker: string
  pct_change: number
  flagged: boolean
}

export interface EveningResponse {
  date: string
  movers: EveningMover[]
  portfolio_value: number | null
  daily_pnl_pct: number | null
}

// ── Picks ─────────────────────────────────────────────────────────────────────

export interface PicksRequest {
  market?: string | null
  strategy?: StrategySpec | null
  top_n?: number | null
}

export interface PicksResponse {
  date: string
  picks: SignalScore[]
  regime: string
}

// ── Backtest ──────────────────────────────────────────────────────────────────

export interface TradeRecord {
  date: string
  ticker: string
  side: 'Buy' | 'Sell'
  shares: number
  price: number
  commission: number
}

export interface BacktestResult {
  daily_equity: [string, number][]
  trades: TradeRecord[]
  final_value: number
  total_return_pct: number
  annualised_return_pct: number
  sharpe_ratio: number
  max_drawdown_pct: number
  /** Share of closed round trips with positive net P&L. */
  win_rate_pct: number
  /** Spearman rank IC per rebalance period. */
  signal_ic_per_period: number[]
  ic_summary?: { n: number; mean: number; std: number; t_stat: number; ic_ir: number; hit_rate: number }
  closed_trades?: {
    ticker: string
    entry_date: string
    exit_date: string
    shares: number
    entry_price: number
    exit_price: number
    pnl: number
    return_pct: number
  }[]
  benchmark_ticker?: string | null
  benchmark_return_pct?: number | null
  /** total_return_pct - benchmark_return_pct */
  alpha_pct?: number | null
  /** Commissions + slippage + impact paid, in currency. */
  total_costs?: number
  turnover_annualised_pct?: number
  avg_holding_days?: number
  n_rebalances?: number
  /** Share of trading days spent fully in cash (macro gate). */
  cash_days_pct?: number
  after_tax_return_pct?: number | null
  notes?: string[]
}

export interface BacktestRequest {
  strategy: StrategySpec
  start_date: string
  end_date: string
  capital: number
}

export interface BacktestResponse {
  strategy_name: string
  result: BacktestResult
  ic_mean: number
}

// ── Strategy history (leaderboard) ────────────────────────────────────────────

export interface StrategyRunRow {
  id: number
  run_at: string
  strategy_name: string
  strategy_spec: string
  total_return: number
  sharpe: number
  max_drawdown: number
  alpha: number
  ic_mean: number
  start_date: string
  end_date: string
}

// ── Paper portfolio ───────────────────────────────────────────────────────────

export interface PaperPosition {
  ticker: string
  shares: number
  entry_price: number
  current_price: number
  pnl_pct: number
  weight: number
}

export interface PaperStatusResponse {
  initialized: boolean
  total_value: number
  cash: number
  cash_pct: number
  invested_pct: number
  total_pnl_pct: number
  positions: PaperPosition[]
  strategy_name: string | null
  last_rebalance: string | null
  next_rebalance: string | null
}

// ── Portfolio history ─────────────────────────────────────────────────────────

export interface EquityPoint {
  date: string
  value: number
  benchmark: number | null
}

export interface PortfolioHistoryResponse {
  points: EquityPoint[]
  total_return_pct: number
  benchmark_return_pct: number | null
}

// ── Correlations ──────────────────────────────────────────────────────────────

export interface CorrelationPair {
  industry_a: string
  industry_b: string
  correlation: number
}

export interface CorrelationsResponse {
  date: string
  pairs: CorrelationPair[]
  top_correlated: CorrelationPair[]
  least_correlated: CorrelationPair[]
  industries: string[]
}

// ── Universe ──────────────────────────────────────────────────────────────────

export interface UniverseResponse {
  date: string
  scores: SignalScore[]
  regime: string
}
