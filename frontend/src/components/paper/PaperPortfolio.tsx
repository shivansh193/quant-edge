'use client'

import { useState } from 'react'
import useSWR from 'swr'
import { RefreshCw, PlayCircle } from 'lucide-react'
import { fetchPaperStatus, initPaper, updatePaper, fetchPresets, parseStrategy } from '@/lib/api'
import {
  Card, CardHeader, Button, StatCard, Badge, Spinner, Empty, ErrorBox, Pct,
} from '@/components/ui'
import type { PaperPosition, PresetStrategy } from '@/lib/types'
import {
  AreaChart, Area, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer,
} from 'recharts'
import { fetchPortfolioHistory } from '@/lib/api'
import clsx from 'clsx'

// ── P&L sparkline (30-day) ────────────────────────────────────────────────────
function PnLChart() {
  const { data } = useSWR('portfolio-history', fetchPortfolioHistory, { revalidateOnFocus: false })
  if (!data || data.points.length === 0) return null
  const pct0 = data.points[0]?.value ?? 1
  const chartData = data.points.map(p => ({
    date: p.date.slice(5),
    pct: ((p.value - pct0) / pct0) * 100,
  }))
  const positive = chartData[chartData.length - 1]?.pct >= 0
  return (
    <ResponsiveContainer width="100%" height={120}>
      <AreaChart data={chartData} margin={{ top: 4, right: 4, bottom: 0, left: 40 }}>
        <defs>
          <linearGradient id="pnlGrad" x1="0" y1="0" x2="0" y2="1">
            <stop offset="5%"  stopColor={positive ? '#00C853' : '#FF1744'} stopOpacity={0.25} />
            <stop offset="95%" stopColor={positive ? '#00C853' : '#FF1744'} stopOpacity={0} />
          </linearGradient>
        </defs>
        <CartesianGrid strokeDasharray="3 3" stroke="#1a1a1a" />
        <XAxis dataKey="date" tick={{ fontSize: 9, fill: '#555', fontFamily: 'JetBrains Mono' }} interval="preserveStartEnd" />
        <YAxis tick={{ fontSize: 9, fill: '#555', fontFamily: 'JetBrains Mono' }} tickFormatter={v => `${v.toFixed(1)}%`} width={38} />
        <Tooltip
          formatter={(v: number) => [`${v.toFixed(2)}%`, 'P&L']}
          contentStyle={{ background: '#1a1a1a', border: '1px solid #333', borderRadius: 4 }}
          labelStyle={{ color: '#888', fontSize: 10 }}
        />
        <Area
          type="monotone"
          dataKey="pct"
          stroke={positive ? '#00C853' : '#FF1744'}
          fill="url(#pnlGrad)"
          strokeWidth={1.5}
          dot={false}
        />
      </AreaChart>
    </ResponsiveContainer>
  )
}

// ── Init form ─────────────────────────────────────────────────────────────────
function InitForm({ onInit }: { onInit: () => void }) {
  const [mode, setMode] = useState<'preset' | 'custom'>('preset')
  const [selectedPreset, setSelectedPreset] = useState('')
  const [nlInput, setNlInput] = useState('')
  const [capital, setCapital] = useState(100000)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const { data: presets } = useSWR('presets', fetchPresets, { revalidateOnFocus: false })

  const submit = async () => {
    setError(null)
    setLoading(true)
    try {
      let spec
      if (mode === 'preset' && selectedPreset) {
        const preset = presets?.find((p: PresetStrategy) => p.id === selectedPreset)
        if (!preset) throw new Error('Preset not found')
        spec = preset.spec
      } else if (mode === 'custom' && nlInput.trim()) {
        spec = await parseStrategy(nlInput.trim())
      } else {
        throw new Error('Select a strategy')
      }
      await initPaper(spec, capital)
      onInit()
    } catch (e: unknown) {
      setError((e as Error).message)
    } finally {
      setLoading(false)
    }
  }

  return (
    <div className="space-y-4">
      <div className="text-muted text-sm font-mono">No active portfolio. Initialize one to start tracking.</div>

      <div className="flex rounded-lg border border-border overflow-hidden w-48">
        {(['preset', 'custom'] as const).map(m => (
          <button key={m} onClick={() => setMode(m)}
            className={clsx('flex-1 py-1.5 text-xs font-mono uppercase tracking-wider transition-colors',
              mode === m ? 'bg-accent/15 text-accent' : 'text-muted hover:text-[#E0E0E0]',
            )}>
            {m}
          </button>
        ))}
      </div>

      {mode === 'preset' ? (
        <select value={selectedPreset} onChange={e => setSelectedPreset(e.target.value)} className="text-xs w-72">
          <option value="">— Select strategy —</option>
          {presets?.map((p: PresetStrategy) => <option key={p.id} value={p.id}>{p.icon} {p.name}</option>)}
        </select>
      ) : (
        <textarea value={nlInput} onChange={e => setNlInput(e.target.value)}
          rows={3} placeholder="Describe your strategy…" className="w-full text-xs resize-none" />
      )}

      <div className="flex items-center gap-3">
        <label className="text-xs text-muted font-mono">Capital ($)</label>
        <input type="number" value={capital} onChange={e => setCapital(Number(e.target.value))}
          step={10000} className="text-xs w-36" />
      </div>

      {error && <ErrorBox message={error} />}

      <Button onClick={submit} loading={loading}>
        <PlayCircle size={13} />
        Initialize Portfolio
      </Button>
    </div>
  )
}

// ── Main ──────────────────────────────────────────────────────────────────────
export default function PaperPortfolio() {
  const { data, isLoading, error, mutate } = useSWR('paper-status', fetchPaperStatus, {
    refreshInterval: 60_000,
    revalidateOnFocus: false,
  })
  const [updating, setUpdating] = useState(false)

  const handleUpdate = async () => {
    setUpdating(true)
    try { await updatePaper(); await mutate() }
    finally { setUpdating(false) }
  }

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="font-mono text-lg font-semibold">Paper Portfolio</h1>
          <p className="text-muted text-sm mt-0.5">Simulated trading — no real money</p>
        </div>
        {data?.initialized && (
          <Button onClick={handleUpdate} loading={updating} variant="ghost">
            <RefreshCw size={13} />
            Update Prices
          </Button>
        )}
      </div>

      {isLoading && <div className="flex justify-center py-12"><Spinner size={24} /></div>}
      {error && <ErrorBox message={error.message} />}

      {data && !data.initialized && (
        <Card>
          <div className="p-6">
            <InitForm onInit={() => mutate()} />
          </div>
        </Card>
      )}

      {data && data.initialized && (
        <>
          {/* Stats row */}
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
            <StatCard
              label="Portfolio Value"
              value={`$${data.total_value.toLocaleString('en-US', { maximumFractionDigits: 0 })}`}
            />
            <StatCard
              label="Total P&L"
              value={`${data.total_pnl_pct >= 0 ? '+' : ''}${data.total_pnl_pct.toFixed(2)}%`}
              color={data.total_pnl_pct >= 0 ? 'green' : 'red'}
            />
            <StatCard label="Cash"      value={`${data.cash_pct.toFixed(1)}%`} />
            <StatCard label="Positions" value={data.positions.length} />
          </div>

          {/* Strategy info */}
          <div className="flex flex-wrap items-center gap-3 text-sm">
            {data.strategy_name && (
              <Badge color="cyan">{data.strategy_name}</Badge>
            )}
            {data.last_rebalance && (
              <span className="text-muted font-mono text-xs">
                Last rebalance: {data.last_rebalance}
              </span>
            )}
            {data.next_rebalance && (
              <span className="text-muted font-mono text-xs">
                Next: {data.next_rebalance}
              </span>
            )}
          </div>

          {/* 30-day chart */}
          <Card>
            <CardHeader title="30-Day P&L" />
            <div className="pt-2 pb-3">
              <PnLChart />
            </div>
          </Card>

          {/* Positions */}
          <Card>
            <CardHeader title="Holdings" subtitle={`${data.positions.length} positions`} />
            {data.positions.length === 0 ? (
              <Empty message="No open positions" />
            ) : (
              <table>
                <thead>
                  <tr>
                    <th>Ticker</th>
                    <th>Weight</th>
                    <th>Shares</th>
                    <th>Entry</th>
                    <th>Current</th>
                    <th>P&L</th>
                  </tr>
                </thead>
                <tbody>
                  {data.positions.map((pos: PaperPosition) => (
                    <tr key={pos.ticker}>
                      <td className="text-accent font-semibold">{pos.ticker}</td>
                      <td>{(pos.weight * 100).toFixed(1)}%</td>
                      <td>{pos.shares.toFixed(2)}</td>
                      <td>${pos.entry_price.toFixed(2)}</td>
                      <td>${pos.current_price.toFixed(2)}</td>
                      <td><Pct value={pos.pnl_pct} /></td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </Card>
        </>
      )}
    </div>
  )
}
