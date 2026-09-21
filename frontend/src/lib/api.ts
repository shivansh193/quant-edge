import type {
  MorningResponse,
  EveningResponse,
  PicksRequest,
  PicksResponse,
  BacktestRequest,
  BacktestResponse,
  StrategySpec,
  PresetStrategy,
  StrategyRunRow,
  PaperStatusResponse,
  PortfolioHistoryResponse,
  CorrelationsResponse,
  UniverseResponse,
  SignalDetailResponse,
} from './types'

// In dev (.env.local): NEXT_PUBLIC_API_BASE_URL=http://localhost:8080
// In prod (static export served by Axum on same origin): leave empty.
const BASE = (process.env.NEXT_PUBLIC_API_BASE_URL ?? '').replace(/\/$/, '')

async function get<T>(path: string): Promise<T> {
  const res = await fetch(`${BASE}${path}`, { cache: 'no-store' })
  if (!res.ok) {
    const text = await res.text().catch(() => res.statusText)
    throw new Error(`${res.status}: ${text}`)
  }
  return res.json() as Promise<T>
}

async function post<T>(path: string, body: unknown): Promise<T> {
  const res = await fetch(`${BASE}${path}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
    cache: 'no-store',
  })
  if (!res.ok) {
    const text = await res.text().catch(() => res.statusText)
    throw new Error(`${res.status}: ${text}`)
  }
  return res.json() as Promise<T>
}

// ── Health ────────────────────────────────────────────────────────────────────

export const fetchHealth = () => get<{ status: string }>('/api/health')

// ── Morning / Evening ─────────────────────────────────────────────────────────

export const fetchMorning    = () => get<MorningResponse>('/api/morning')
export const fetchEvening    = () => get<EveningResponse>('/api/evening')

// ── Picks ─────────────────────────────────────────────────────────────────────

export const fetchPicks = (body: PicksRequest) =>
  post<PicksResponse>('/api/picks', body)

// ── Strategies ────────────────────────────────────────────────────────────────

export const fetchPresets = () =>
  get<PresetStrategy[]>('/api/strategies/presets')

export const runPreset = (preset_id: string, top_n = 10) =>
  post<PicksResponse>('/api/strategies/run', { preset_id, top_n })

export const parseStrategy = (description: string) =>
  post<StrategySpec>('/api/strategy/parse', { description })

// ── Backtest ──────────────────────────────────────────────────────────────────

export const runBacktest = (body: BacktestRequest) =>
  post<BacktestResponse>('/api/backtest', body)

export const fetchStrategyHistory = () =>
  get<StrategyRunRow[]>('/api/strategies/history')

// ── Paper portfolio ───────────────────────────────────────────────────────────

export const fetchPaperStatus = () =>
  get<PaperStatusResponse>('/api/paper/status')

export const initPaper = (strategy: StrategySpec, capital: number) =>
  post<{ message: string }>('/api/paper/init', { strategy, capital })

export const updatePaper = () =>
  post<{ message: string }>('/api/paper/update', {})

// ── Portfolio history ─────────────────────────────────────────────────────────

export const fetchPortfolioHistory = () =>
  get<PortfolioHistoryResponse>('/api/portfolio/history')

// ── Correlations ──────────────────────────────────────────────────────────────

export const fetchCorrelations = () =>
  get<CorrelationsResponse>('/api/correlations')

// ── Universe ──────────────────────────────────────────────────────────────────

export const fetchUniverse = () =>
  get<UniverseResponse>('/api/universe')

// ── Signal detail ─────────────────────────────────────────────────────────────

export const fetchSignalDetail = (ticker: string) =>
  get<SignalDetailResponse>(`/api/signals/${encodeURIComponent(ticker)}`)
