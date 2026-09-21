'use client'

import { useState, useMemo } from 'react'
import useSWR from 'swr'
import { RefreshCw, Search } from 'lucide-react'
import { fetchUniverse } from '@/lib/api'
import {
  Card, CardHeader, Button, Badge, Spinner, Empty, ErrorBox, ScoreBar,
} from '@/components/ui'
import SignalModal from '@/components/SignalModal'
import type { SignalScore } from '@/lib/types'
import clsx from 'clsx'

export default function UniverseExplorer() {
  const [search, setSearch] = useState('')
  const [market, setMarket] = useState('ALL')
  const [selected, setSelected] = useState<string | null>(null)

  const { data, error, isLoading, mutate } = useSWR('universe', fetchUniverse, {
    revalidateOnFocus: false,
  })

  const filtered = useMemo(() => {
    if (!data) return []
    let rows = data.scores
    if (market === 'US') rows = rows.filter(r => !r.ticker.endsWith('.NS'))
    if (market === 'IN') rows = rows.filter(r => r.ticker.endsWith('.NS'))
    if (search.trim()) {
      const q = search.trim().toUpperCase()
      rows = rows.filter(r =>
        r.ticker.includes(q) || r.industry.toUpperCase().includes(q)
      )
    }
    return rows
  }, [data, market, search])

  return (
    <div className="space-y-6">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div>
          <h1 className="font-mono text-lg font-semibold">Universe Explorer</h1>
          <p className="text-muted text-sm mt-0.5">
            {data
              ? `${data.scores.length} tickers · ${data.date} · ${data.regime}`
              : 'Score all tickers in the universe'}
          </p>
        </div>
        <Button onClick={() => mutate()} loading={isLoading}>
          <RefreshCw size={13} />
          Refresh
        </Button>
      </div>

      {error && <ErrorBox message={error.message} />}

      {/* Filter bar */}
      <div className="flex items-center gap-3">
        <div className="relative">
          <Search size={13} className="absolute left-2.5 top-1/2 -translate-y-1/2 text-muted" />
          <input
            type="text"
            placeholder="Search ticker or industry…"
            value={search}
            onChange={e => setSearch(e.target.value)}
            className="pl-8 pr-3 py-1.5 text-sm w-64"
          />
        </div>
        <div className="flex rounded-lg border border-border overflow-hidden">
          {(['ALL', 'US', 'IN'] as const).map(m => (
            <button
              key={m}
              onClick={() => setMarket(m)}
              className={clsx(
                'px-3 py-1.5 text-xs font-mono uppercase tracking-wider transition-colors',
                market === m ? 'bg-accent/15 text-accent' : 'text-muted hover:text-[#E0E0E0]',
              )}
            >
              {m}
            </button>
          ))}
        </div>
        {data && (
          <Badge color={data.regime === 'risk_on' ? 'green' : 'red'}>
            {data.regime}
          </Badge>
        )}
      </div>

      {/* Table */}
      <Card>
        {isLoading && (
          <div className="flex items-center justify-center h-48">
            <Spinner size={24} />
          </div>
        )}
        {!isLoading && (!data || filtered.length === 0) && (
          <Empty message={search ? 'No matches' : 'Click Refresh to load scores'} />
        )}
        {!isLoading && filtered.length > 0 && (
          <table>
            <thead>
              <tr>
                <th className="w-8">#</th>
                <th>Ticker</th>
                <th>Industry</th>
                <th>Score</th>
                <th>Momentum</th>
                <th>Fundamental</th>
                <th>Insider</th>
                <th>Sentiment</th>
                <th>Pairs</th>
                <th>Macro</th>
              </tr>
            </thead>
            <tbody>
              {filtered.slice(0, 200).map((row: SignalScore) => (
                <tr
                  key={row.ticker}
                  className="cursor-pointer"
                  onClick={() => setSelected(row.ticker)}
                >
                  <td className="text-muted">{row.rank}</td>
                  <td className="text-accent font-semibold">{row.ticker}</td>
                  <td className="text-muted max-w-[160px] truncate text-xs">{row.industry}</td>
                  <td className="w-28"><ScoreBar value={row.composite} /></td>
                  <td className="text-xs">{row.momentum_contrib.toFixed(0)}</td>
                  <td className="text-xs">{row.fundamental_contrib.toFixed(0)}</td>
                  <td className="text-xs">{row.insider_contrib.toFixed(0)}</td>
                  <td className="text-xs">{row.sentiment_contrib.toFixed(0)}</td>
                  <td className="text-xs">{row.pairs_contrib.toFixed(0)}</td>
                  <td>
                    <Badge color={row.macro_on ? 'green' : 'red'}>
                      {row.macro_on ? 'ON' : 'OFF'}
                    </Badge>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        {filtered.length > 200 && (
          <div className="px-4 py-2 text-xs text-muted font-mono border-t border-border">
            Showing 200 of {filtered.length} — narrow the search to see more
          </div>
        )}
      </Card>

      <SignalModal ticker={selected} onClose={() => setSelected(null)} />
    </div>
  )
}
