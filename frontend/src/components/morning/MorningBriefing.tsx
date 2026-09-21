'use client'

import { useState } from 'react'
import useSWR from 'swr'
import { RefreshCw, TrendingUp, TrendingDown, Minus, AlertTriangle } from 'lucide-react'
import { fetchMorning } from '@/lib/api'
import { Card, CardHeader, Button, StatCard, Badge, Spinner, Empty, ErrorBox, ScoreBar, Pct } from '@/components/ui'
import SignalModal from '@/components/SignalModal'
import type { PickSummary } from '@/lib/types'

function macroColor(regime: string) {
  if (regime === 'risk_on')  return 'text-positive'
  if (regime === 'risk_off') return 'text-negative'
  return 'text-warning'
}

function TrendIcon({ v }: { v: number | null | undefined }) {
  if (v == null) return <Minus size={12} className="text-muted" />
  if (v > 0.5)  return <TrendingUp size={12} className="text-positive" />
  if (v < -0.5) return <TrendingDown size={12} className="text-negative" />
  return <Minus size={12} className="text-muted" />
}

export default function MorningBriefing() {
  const [selected, setSelected] = useState<string | null>(null)
  const { data, error, isLoading, mutate } = useSWR('morning', fetchMorning, {
    revalidateOnFocus: false,
  })

  return (
    <div className="space-y-6">
      {/* Page header */}
      <div className="flex items-center justify-between">
        <div>
          <h1 className="font-mono text-lg font-semibold text-[#E0E0E0]">Morning Briefing</h1>
          <p className="text-muted text-sm mt-0.5">
            {data ? data.date : 'Loading…'}
          </p>
        </div>
        <Button
          onClick={() => mutate()}
          loading={isLoading}
          variant="primary"
        >
          <RefreshCw size={13} />
          Run Scan
        </Button>
      </div>

      {error && <ErrorBox message={error.message} />}

      {/* Macro strip */}
      {data && (
        <div className="grid grid-cols-2 gap-3 sm:grid-cols-4 xl:grid-cols-7">
          <StatCard
            label="Regime"
            value={data.macro.regime.replace('_', ' ').toUpperCase()}
            color={data.macro.regime === 'risk_on' ? 'green' : data.macro.regime === 'risk_off' ? 'red' : undefined}
          />
          {data.macro.vix != null && (
            <StatCard
              label="VIX"
              value={data.macro.vix.toFixed(1)}
              color={data.macro.vix > 25 ? 'red' : data.macro.vix < 15 ? 'green' : undefined}
              sub={data.macro.vix > 25 ? 'Elevated' : 'Normal'}
            />
          )}
          {data.macro.yield_10y != null && (
            <StatCard
              label="10Y Yield"
              value={`${data.macro.yield_10y.toFixed(2)}%`}
            />
          )}
          {data.macro.yield_2y != null && (
            <StatCard
              label="2Y Yield"
              value={`${data.macro.yield_2y.toFixed(2)}%`}
              sub={data.macro.yield_10y != null
                ? `Spread ${((data.macro.yield_10y - data.macro.yield_2y) * 100).toFixed(0)}bp`
                : undefined}
            />
          )}
          {data.macro.sp500_1d_pct != null && (
            <StatCard
              label="S&P 500"
              value={`${data.macro.sp500_1d_pct >= 0 ? '+' : ''}${data.macro.sp500_1d_pct.toFixed(2)}%`}
              color={data.macro.sp500_1d_pct >= 0 ? 'green' : 'red'}
            />
          )}
          {data.macro.nifty50_1d_pct != null && (
            <StatCard
              label="Nifty 50"
              value={`${data.macro.nifty50_1d_pct >= 0 ? '+' : ''}${data.macro.nifty50_1d_pct.toFixed(2)}%`}
              color={data.macro.nifty50_1d_pct >= 0 ? 'green' : 'red'}
            />
          )}
          {data.macro.usdinr != null && (
            <StatCard label="USD/INR" value={data.macro.usdinr.toFixed(2)} />
          )}
        </div>
      )}

      {/* Macro warning when risk_off */}
      {data?.macro.regime === 'risk_off' && (
        <div className="flex items-center gap-2 bg-negative/10 border border-negative/20 rounded-lg px-4 py-2.5 text-sm text-negative">
          <AlertTriangle size={14} />
          Risk-off regime active — the macro gate recommends holding cash: no new long picks.
        </div>
      )}

      <div className="grid grid-cols-3 gap-5">
        {/* Top picks table */}
        <div className="col-span-2">
          <Card>
            <CardHeader
              title="Top Picks"
              subtitle={data ? `${data.universe_size} tickers scored` : undefined}
            />
            {isLoading && (
              <div className="flex items-center justify-center h-40">
                <Spinner size={22} />
              </div>
            )}
            {!isLoading && (!data || data.top_picks.length === 0) && (
              <Empty message="Run a scan to load picks" />
            )}
            {data && data.top_picks.length > 0 && (
              <table>
                <thead>
                  <tr>
                    <th className="w-10">#</th>
                    <th>Ticker</th>
                    <th>Industry</th>
                    <th>Score</th>
                    <th>1D Chg</th>
                    <th>Macro</th>
                  </tr>
                </thead>
                <tbody>
                  {data.top_picks.map((pick: PickSummary) => (
                    <tr
                      key={pick.ticker}
                      className="cursor-pointer"
                      onClick={() => setSelected(pick.ticker)}
                    >
                      <td className="text-muted">{pick.rank}</td>
                      <td className="text-accent font-semibold">{pick.ticker}</td>
                      <td className="text-muted max-w-[180px] truncate">{pick.industry}</td>
                      <td className="w-36">
                        <ScoreBar value={pick.score} />
                      </td>
                      <td>
                        <div className="flex items-center gap-1">
                          <TrendIcon v={pick.price_change_1d_pct} />
                          <Pct value={pick.price_change_1d_pct} />
                        </div>
                      </td>
                      <td>
                        <Badge color={pick.macro_on ? 'green' : 'red'}>
                          {pick.macro_on ? 'ON' : 'OFF'}
                        </Badge>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </Card>
        </div>

        {/* Paper snapshot */}
        <div>
          <Card>
            <CardHeader title="Paper Portfolio" />
            {data?.paper_snapshot ? (
              <div className="p-4 space-y-4">
                <div>
                  <div className="text-[10px] text-muted uppercase tracking-wider font-mono mb-1">Portfolio Value</div>
                  <div className="font-mono text-2xl font-bold text-[#E0E0E0]">
                    ${data.paper_snapshot.total_value.toLocaleString('en-US', { minimumFractionDigits: 0, maximumFractionDigits: 0 })}
                  </div>
                </div>

                <div className="grid grid-cols-2 gap-3">
                  <div className="bg-[#111] rounded p-3">
                    <div className="text-[10px] text-muted font-mono mb-1">P&L</div>
                    <Pct value={data.paper_snapshot.pnl_pct} decimals={2} />
                  </div>
                  <div className="bg-[#111] rounded p-3">
                    <div className="text-[10px] text-muted font-mono mb-1">Positions</div>
                    <div className="font-mono text-sm">{data.paper_snapshot.position_count}</div>
                  </div>
                  <div className="bg-[#111] rounded p-3">
                    <div className="text-[10px] text-muted font-mono mb-1">Cash</div>
                    <div className="font-mono text-sm">{data.paper_snapshot.cash_pct.toFixed(1)}%</div>
                  </div>
                  {data.paper_snapshot.last_rebalance && (
                    <div className="bg-[#111] rounded p-3">
                      <div className="text-[10px] text-muted font-mono mb-1">Rebalanced</div>
                      <div className="font-mono text-xs">{data.paper_snapshot.last_rebalance}</div>
                    </div>
                  )}
                </div>
              </div>
            ) : (
              <Empty message="No paper portfolio" />
            )}
          </Card>
        </div>
      </div>

      <SignalModal ticker={selected} onClose={() => setSelected(null)} />
    </div>
  )
}
