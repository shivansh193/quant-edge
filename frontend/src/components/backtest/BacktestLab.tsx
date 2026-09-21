'use client'

import { useState } from 'react'
import useSWR from 'swr'
import { Play, Trophy } from 'lucide-react'
import { runBacktest, fetchStrategyHistory, parseStrategy, fetchPresets } from '@/lib/api'
import {
  Card, CardHeader, Button, StatCard, Badge, Spinner, Empty, ErrorBox, Pct,
} from '@/components/ui'
import type { BacktestResponse, StrategyRunRow, PresetStrategy } from '@/lib/types'
import {
  LineChart, Line, XAxis, YAxis, CartesianGrid, Tooltip,
  ResponsiveContainer, ReferenceLine,
} from 'recharts'
import clsx from 'clsx'

// ── Equity curve chart ────────────────────────────────────────────────────────
function EquityCurve({ equity }: { equity: [string, number][] }) {
  const data = equity.map(([date, value]) => ({ date, value }))
  const initial = data[0]?.value ?? 100_000
  const final   = data[data.length - 1]?.value ?? initial
  const gain    = final >= initial

  return (
    <ResponsiveContainer width="100%" height={260}>
      <LineChart data={data} margin={{ top: 4, right: 12, bottom: 4, left: 60 }}>
        <CartesianGrid strokeDasharray="3 3" stroke="#1E1E1E" />
        <XAxis
          dataKey="date"
          tick={{ fontSize: 10, fill: '#666', fontFamily: 'JetBrains Mono' }}
          tickFormatter={d => d.slice(5)}   // MM-DD
          interval="preserveStartEnd"
        />
        <YAxis
          tick={{ fontSize: 10, fill: '#666', fontFamily: 'JetBrains Mono' }}
          tickFormatter={v => `$${(v / 1000).toFixed(0)}k`}
          width={55}
        />
        <Tooltip
          formatter={(v: number) => [`$${v.toLocaleString('en-US', { minimumFractionDigits: 0 })}`, 'Portfolio']}
          labelFormatter={l => `Date: ${l}`}
          contentStyle={{ background: '#1a1a1a', border: '1px solid #333', borderRadius: 4 }}
          labelStyle={{ color: '#888', fontSize: 11 }}
        />
        <ReferenceLine y={initial} stroke="#333" strokeDasharray="4 2" />
        <Line
          type="monotone"
          dataKey="value"
          stroke={gain ? '#00C853' : '#FF1744'}
          strokeWidth={1.5}
          dot={false}
          activeDot={{ r: 4, fill: gain ? '#00C853' : '#FF1744' }}
        />
      </LineChart>
    </ResponsiveContainer>
  )
}

// ── Leaderboard table ─────────────────────────────────────────────────────────
function Leaderboard() {
  const { data, isLoading } = useSWR('strategy-history', fetchStrategyHistory, {
    revalidateOnFocus: false,
  })
  if (isLoading) return <div className="flex justify-center py-6"><Spinner /></div>
  if (!data || data.length === 0) return <Empty message="No runs yet" />
  return (
    <table>
      <thead>
        <tr>
          <th className="w-6">#</th>
          <th>Strategy</th>
          <th>Return</th>
          <th>Sharpe</th>
          <th>Max DD</th>
          <th>IC</th>
          <th>Period</th>
        </tr>
      </thead>
      <tbody>
        {data.map((row: StrategyRunRow, i) => (
          <tr key={row.id}>
            <td className="text-muted">{i + 1}</td>
            <td className="text-accent max-w-[160px] truncate">{row.strategy_name}</td>
            <td><Pct value={row.total_return} /></td>
            <td className={clsx('font-mono',
              row.sharpe >= 1.5 ? 'text-positive' : row.sharpe >= 0.5 ? 'text-[#E0E0E0]' : 'text-negative'
            )}>
              {row.sharpe.toFixed(2)}
            </td>
            <td className="text-negative">-{row.max_drawdown.toFixed(1)}%</td>
            <td className="text-muted">{row.ic_mean.toFixed(3)}</td>
            <td className="text-muted text-[10px]">
              {row.start_date.slice(0, 7)} → {row.end_date.slice(0, 7)}
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}

// ── Main component ────────────────────────────────────────────────────────────
export default function BacktestLab() {
  const [strategyMode, setStrategyMode] = useState<'preset' | 'custom'>('preset')
  const [selectedPreset, setSelectedPreset] = useState('')
  const [nlInput, setNlInput] = useState('')
  const [startDate, setStartDate] = useState('2022-01-01')
  const [endDate, setEndDate] = useState('2024-01-01')
  const [capital, setCapital] = useState(100000)
  const [running, setRunning] = useState(false)
  const [result, setResult] = useState<BacktestResponse | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [activeTab, setActiveTab] = useState<'chart' | 'trades' | 'leaderboard'>('chart')

  const { data: presets } = useSWR('presets', fetchPresets, { revalidateOnFocus: false })

  const run = async () => {
    setError(null)
    setRunning(true)
    try {
      let spec
      if (strategyMode === 'preset' && selectedPreset) {
        const preset = presets?.find((p: PresetStrategy) => p.id === selectedPreset)
        if (!preset) throw new Error('Preset not found')
        spec = preset.spec
      } else if (strategyMode === 'custom' && nlInput.trim()) {
        spec = await parseStrategy(nlInput.trim())
      } else {
        throw new Error('Select a preset or enter a strategy description')
      }
      const res = await runBacktest({ strategy: spec, start_date: startDate, end_date: endDate, capital })
      setResult(res)
      setActiveTab('chart')
    } catch (e: unknown) {
      setError((e as Error).message)
    } finally {
      setRunning(false)
    }
  }

  return (
    <div className="space-y-6">
      <div>
        <h1 className="font-mono text-lg font-semibold">Backtest Lab</h1>
        <p className="text-muted text-sm mt-0.5">Replay any strategy over historical data</p>
      </div>

      <div className="grid grid-cols-3 gap-5">
        {/* Config panel */}
        <div className="col-span-1 space-y-4">
          <Card>
            <CardHeader title="Configuration" />
            <div className="p-4 space-y-4">
              {/* Strategy mode */}
              <div className="flex rounded-lg border border-border overflow-hidden">
                {(['preset', 'custom'] as const).map(m => (
                  <button key={m} onClick={() => setStrategyMode(m)}
                    className={clsx('flex-1 py-1.5 text-xs font-mono uppercase tracking-wider transition-colors',
                      strategyMode === m ? 'bg-accent/15 text-accent' : 'text-muted hover:text-[#E0E0E0]',
                    )}>
                    {m}
                  </button>
                ))}
              </div>

              {strategyMode === 'preset' ? (
                <div>
                  <label className="text-xs text-muted font-mono block mb-1.5">Strategy</label>
                  <select
                    value={selectedPreset}
                    onChange={e => setSelectedPreset(e.target.value)}
                    className="w-full text-xs"
                  >
                    <option value="">— Select —</option>
                    {presets?.map((p: PresetStrategy) => (
                      <option key={p.id} value={p.id}>{p.icon} {p.name}</option>
                    ))}
                  </select>
                </div>
              ) : (
                <div>
                  <label className="text-xs text-muted font-mono block mb-1.5">Strategy description</label>
                  <textarea
                    value={nlInput}
                    onChange={e => setNlInput(e.target.value)}
                    rows={4}
                    placeholder="Describe your strategy…"
                    className="w-full text-xs resize-none"
                  />
                </div>
              )}

              <div className="space-y-3">
                <div>
                  <label className="text-xs text-muted font-mono block mb-1">Start date</label>
                  <input type="date" value={startDate} onChange={e => setStartDate(e.target.value)} className="w-full text-xs" />
                </div>
                <div>
                  <label className="text-xs text-muted font-mono block mb-1">End date</label>
                  <input type="date" value={endDate} onChange={e => setEndDate(e.target.value)} className="w-full text-xs" />
                </div>
                <div>
                  <label className="text-xs text-muted font-mono block mb-1">Capital ($)</label>
                  <input
                    type="number"
                    value={capital}
                    onChange={e => setCapital(Number(e.target.value))}
                    className="w-full text-xs"
                    step={10000}
                  />
                </div>
              </div>

              <Button className="w-full justify-center" onClick={run} loading={running}>
                <Play size={13} />
                Run Backtest
              </Button>

              {error && <ErrorBox message={error} />}
            </div>
          </Card>
        </div>

        {/* Results panel */}
        <div className="col-span-2 space-y-4">
          {/* Metrics */}
          {result && (
            <div className="grid grid-cols-4 gap-3">
              <StatCard
                label="Total Return"
                value={`${result.result.total_return_pct >= 0 ? '+' : ''}${result.result.total_return_pct.toFixed(1)}%`}
                color={result.result.total_return_pct >= 0 ? 'green' : 'red'}
              />
              <StatCard
                label="Ann. Return"
                value={`${result.result.annualised_return_pct >= 0 ? '+' : ''}${result.result.annualised_return_pct.toFixed(1)}%`}
                color={result.result.annualised_return_pct >= 0 ? 'green' : 'red'}
              />
              <StatCard
                label="Sharpe"
                value={result.result.sharpe_ratio.toFixed(2)}
                color={result.result.sharpe_ratio >= 1 ? 'green' : undefined}
              />
              <StatCard
                label="Max Drawdown"
                value={`-${result.result.max_drawdown_pct.toFixed(1)}%`}
                color="red"
              />
              <StatCard label="Win Rate"   value={`${result.result.win_rate_pct.toFixed(1)}%`} />
              <StatCard label="Trades"     value={result.result.trades.length} />
              <StatCard label="IC Mean"    value={result.ic_mean.toFixed(3)} />
              <StatCard label="Final Value" value={`$${result.result.final_value.toLocaleString('en-US', { maximumFractionDigits: 0 })}`} />
            </div>
          )}

          {/* Tabs */}
          <Card>
            <div className="flex border-b border-border">
              {(['chart', 'trades', 'leaderboard'] as const).map(tab => (
                <button
                  key={tab}
                  onClick={() => setActiveTab(tab)}
                  className={clsx(
                    'px-4 py-2.5 text-xs font-mono uppercase tracking-wider transition-colors capitalize',
                    activeTab === tab
                      ? 'border-b-2 border-accent text-accent -mb-px'
                      : 'text-muted hover:text-[#E0E0E0]',
                  )}
                >
                  {tab === 'leaderboard' && <Trophy size={11} className="inline mr-1" />}
                  {tab}
                </button>
              ))}
            </div>

            <div>
              {activeTab === 'chart' && (
                <>
                  {running && <div className="flex justify-center h-48 items-center"><Spinner size={22} /></div>}
                  {!running && !result && <Empty message="Run a backtest to see the equity curve" />}
                  {!running && result && (
                    <div className="pt-4 pb-2">
                      <div className="text-[10px] text-muted font-mono px-4 mb-2 uppercase tracking-wider">
                        {result.strategy_name} · equity curve
                      </div>
                      <EquityCurve equity={result.result.daily_equity} />
                    </div>
                  )}
                </>
              )}

              {activeTab === 'trades' && (
                <>
                  {!result && <Empty message="No trades yet" />}
                  {result && (
                    <table>
                      <thead>
                        <tr>
                          <th>Date</th>
                          <th>Ticker</th>
                          <th>Side</th>
                          <th>Shares</th>
                          <th>Price</th>
                          <th>Commission</th>
                        </tr>
                      </thead>
                      <tbody>
                        {result.result.trades.slice(0, 200).map((t, i) => (
                          <tr key={i}>
                            <td>{t.date}</td>
                            <td className="text-accent">{t.ticker}</td>
                            <td>
                              <Badge color={t.side === 'Buy' ? 'green' : 'red'}>{t.side}</Badge>
                            </td>
                            <td>{t.shares.toFixed(2)}</td>
                            <td>${t.price.toFixed(2)}</td>
                            <td className="text-muted">${t.commission.toFixed(2)}</td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  )}
                </>
              )}

              {activeTab === 'leaderboard' && <Leaderboard />}
            </div>
          </Card>
        </div>
      </div>
    </div>
  )
}
