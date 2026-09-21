'use client'

import { useEffect, useState } from 'react'
import { X } from 'lucide-react'
import useSWR from 'swr'
import { fetchSignalDetail } from '@/lib/api'
import { Spinner, Badge, ScoreBar } from './ui'
import type { FundamentalSnapshot } from '@/lib/types'
import {
  RadarChart, PolarGrid, PolarAngleAxis, Radar,
  ResponsiveContainer, Tooltip,
} from 'recharts'

interface Props {
  ticker: string | null
  onClose: () => void
}

function fmtBig(v: number | null | undefined): string {
  if (v == null) return '—'
  if (Math.abs(v) >= 1e9) return `${(v / 1e9).toFixed(1)}B`
  if (Math.abs(v) >= 1e6) return `${(v / 1e6).toFixed(1)}M`
  return v.toFixed(2)
}

function FundRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex justify-between py-1.5 border-b border-[#1a1a1a] last:border-0">
      <span className="text-muted text-xs">{label}</span>
      <span className="font-mono text-xs text-[#E0E0E0]">{value}</span>
    </div>
  )
}

export default function SignalModal({ ticker, onClose }: Props) {
  const { data, error, isLoading } = useSWR(
    ticker ? ['signal', ticker] : null,
    () => fetchSignalDetail(ticker!),
    { revalidateOnFocus: false },
  )

  // Close on Escape
  useEffect(() => {
    const handler = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose() }
    document.addEventListener('keydown', handler)
    return () => document.removeEventListener('keydown', handler)
  }, [onClose])

  if (!ticker) return null

  const radarData = data
    ? [
        { signal: 'Momentum',    value: ((data.score.momentum_raw + 1) / 2) * 100 },
        { signal: 'Fundamental', value: ((data.score.fundamental_raw + 1) / 2) * 100 },
        { signal: 'Insider',     value: ((data.score.insider_raw + 1) / 2) * 100 },
        { signal: 'Sentiment',   value: ((data.score.sentiment_raw + 1) / 2) * 100 },
        { signal: 'Pairs',       value: ((data.score.pairs_raw + 1) / 2) * 100 },
      ]
    : []

  const f: FundamentalSnapshot | null = data?.fundamentals ?? null

  return (
    <div
      className="fixed inset-0 bg-black/70 backdrop-blur-sm z-50 flex items-center justify-center p-4"
      onClick={onClose}
    >
      <div
        className="bg-panel border border-border rounded-xl w-full max-w-2xl max-h-[90vh] overflow-y-auto"
        onClick={e => e.stopPropagation()}
      >
        {/* Header */}
        <div className="flex items-center justify-between px-5 py-4 border-b border-border">
          <div>
            <span className="font-mono font-bold text-lg text-accent">{ticker}</span>
            {data && (
              <span className="ml-3 text-muted text-sm">{data.score.industry}</span>
            )}
          </div>
          <button onClick={onClose} className="text-muted hover:text-[#E0E0E0] transition-colors">
            <X size={18} />
          </button>
        </div>

        {isLoading && (
          <div className="flex items-center justify-center h-48">
            <Spinner size={24} />
          </div>
        )}

        {error && (
          <div className="p-5 text-negative font-mono text-sm">
            {error.message}
          </div>
        )}

        {data && (
          <div className="p-5 grid grid-cols-2 gap-5">
            {/* Radar chart */}
            <div>
              <div className="text-[10px] text-muted uppercase tracking-wider mb-2 font-mono">Signal Radar</div>
              <ResponsiveContainer width="100%" height={220}>
                <RadarChart data={radarData}>
                  <PolarGrid stroke="#222" />
                  <PolarAngleAxis
                    dataKey="signal"
                    tick={{ fontSize: 10, fill: '#666', fontFamily: 'JetBrains Mono' }}
                  />
                  <Tooltip
                    formatter={(v: number) => [`${v.toFixed(1)}`, '']}
                    contentStyle={{ background: '#1a1a1a', border: '1px solid #333', borderRadius: 4 }}
                    labelStyle={{ color: '#888', fontSize: 11 }}
                  />
                  <Radar
                    dataKey="value"
                    stroke="#00E5FF"
                    fill="#00E5FF"
                    fillOpacity={0.15}
                    strokeWidth={1.5}
                  />
                </RadarChart>
              </ResponsiveContainer>

              {/* Composite score */}
              <div className="mt-3">
                <div className="flex justify-between text-xs mb-1">
                  <span className="text-muted font-mono">Composite score</span>
                  <span className="font-mono text-accent font-bold">{data.score.composite.toFixed(1)}</span>
                </div>
                <ScoreBar value={data.score.composite} />
              </div>

              {/* Signal breakdown */}
              <div className="mt-4 space-y-2">
                {(['momentum', 'fundamental', 'insider', 'sentiment', 'pairs'] as const).map(sig => (
                  <div key={sig}>
                    <div className="flex justify-between text-[10px] text-muted mb-0.5 font-mono uppercase">
                      <span>{sig}</span>
                      <span>{(data.score[`${sig}_contrib`]).toFixed(1)}</span>
                    </div>
                    <ScoreBar value={data.score[`${sig}_contrib`]} />
                  </div>
                ))}
              </div>
            </div>

            {/* Fundamentals */}
            <div>
              <div className="text-[10px] text-muted uppercase tracking-wider mb-2 font-mono">Fundamentals</div>
              {f ? (
                <div className="bg-[#111] rounded-lg px-3 py-2">
                  <FundRow label="Revenue TTM"     value={fmtBig(f.revenue_ttm)} />
                  <FundRow label="Revenue CAGR 3Y" value={f.revenue_cagr_3yr != null ? `${(f.revenue_cagr_3yr * 100).toFixed(1)}%` : '—'} />
                  <FundRow label="Net Margin"      value={f.net_margin_pct != null ? `${f.net_margin_pct.toFixed(1)}%` : '—'} />
                  <FundRow label="Gross Margin"    value={f.gross_profit_margin != null ? `${f.gross_profit_margin.toFixed(1)}%` : '—'} />
                  <FundRow label="ROA"             value={f.return_on_assets != null ? `${f.return_on_assets.toFixed(1)}%` : '—'} />
                  <FundRow label="Oper. Cashflow"  value={fmtBig(f.operating_cashflow)} />
                  <FundRow label="Debt / Equity"   value={f.debt_to_equity != null ? `${f.debt_to_equity.toFixed(1)}%` : '—'} />
                  <FundRow label="Price / Book"    value={f.price_to_book != null ? f.price_to_book.toFixed(2) : '—'} />
                  <FundRow label="12m-1m Return"   value={f.price_return_12m_1m != null ? `${(f.price_return_12m_1m * 100).toFixed(1)}%` : '—'} />
                  <FundRow label="Market Share"    value={f.market_share_proxy != null ? `${(f.market_share_proxy * 100).toFixed(1)}%` : '—'} />
                </div>
              ) : (
                <div className="text-muted text-sm font-mono">No fundamentals cached</div>
              )}

              {/* Data sources */}
              <div className="mt-4 text-[10px] text-muted uppercase tracking-wider mb-2 font-mono">Data quality</div>
              <div className="flex flex-wrap gap-2">
                <Badge color={data.score.macro_on ? 'green' : 'red'}>
                  Macro {data.score.macro_on ? 'ON' : 'OFF'}
                </Badge>
                <Badge color={data.insider_trade_count > 0 ? 'cyan' : 'default'}>
                  {data.insider_trade_count} insider
                </Badge>
                <Badge color={data.news_count > 0 ? 'cyan' : 'default'}>
                  {data.news_count} news
                </Badge>
                <Badge color={data.reddit_mention_count > 0 ? 'cyan' : 'default'}>
                  {data.reddit_mention_count} reddit
                </Badge>
              </div>
            </div>
          </div>
        )}
      </div>
    </div>
  )
}
