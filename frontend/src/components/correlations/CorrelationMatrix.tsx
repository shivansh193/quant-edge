'use client'

import useSWR from 'swr'
import { RefreshCw } from 'lucide-react'
import { fetchCorrelations } from '@/lib/api'
import {
  Card, CardHeader, Button, Spinner, Empty, ErrorBox,
} from '@/components/ui'
import type { CorrelationPair } from '@/lib/types'

// ── Correlation bar ───────────────────────────────────────────────────────────
function CorrelBar({ value }: { value: number }) {
  const abs = Math.abs(value)
  const color = value > 0.7
    ? '#FF1744'
    : value > 0.3
    ? '#FFD600'
    : value < -0.3
    ? '#00E5FF'
    : '#666'
  return (
    <div className="flex items-center gap-2">
      <div className="relative w-28 h-2 bg-white/5 rounded-full overflow-hidden">
        {/* Zero line */}
        <div className="absolute top-0 bottom-0 left-1/2 w-px bg-[#333]" />
        {/* Bar */}
        <div
          style={{
            width: `${abs * 50}%`,
            background: color,
            left: value >= 0 ? '50%' : `${(1 - abs) * 50}%`,
          }}
          className="absolute top-0 bottom-0 rounded-sm"
        />
      </div>
      <span className="font-mono text-xs w-12" style={{ color }}>
        {value.toFixed(3)}
      </span>
    </div>
  )
}

// ── Pairs table ───────────────────────────────────────────────────────────────
function PairsTable({ pairs, title }: { pairs: CorrelationPair[]; title: string }) {
  return (
    <Card>
      <CardHeader title={title} subtitle={`${pairs.length} pairs`} />
      {pairs.length === 0 ? (
        <Empty />
      ) : (
        <table>
          <thead>
            <tr>
              <th>Industry A</th>
              <th>Industry B</th>
              <th>Correlation</th>
            </tr>
          </thead>
          <tbody>
            {pairs.map((p, i) => (
              <tr key={i}>
                <td className="max-w-[180px] truncate text-xs">{p.industry_a}</td>
                <td className="max-w-[180px] truncate text-xs">{p.industry_b}</td>
                <td><CorrelBar value={p.correlation} /></td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Card>
  )
}

// ── Mini heatmap (top N industries) ──────────────────────────────────────────
function MiniHeatmap({ industries, pairs }: { industries: string[]; pairs: CorrelationPair[] }) {
  const top = industries.slice(0, 12)
  // Build lookup map
  const lookup = new Map<string, number>()
  for (const p of pairs) {
    lookup.set(`${p.industry_a}||${p.industry_b}`, p.correlation)
    lookup.set(`${p.industry_b}||${p.industry_a}`, p.correlation)
  }

  const cellColor = (v: number | undefined) => {
    if (v == null) return '#1a1a1a'
    if (v === 1.0) return '#0D0D0D'
    if (v >  0.7) return 'rgba(255,23,68,0.6)'
    if (v >  0.4) return 'rgba(255,214,0,0.4)'
    if (v >  0.0) return 'rgba(255,214,0,0.15)'
    if (v < -0.3) return 'rgba(0,229,255,0.35)'
    return '#1a1a1a'
  }

  const short = (s: string) => s.length > 14 ? s.slice(0, 13) + '…' : s

  return (
    <Card>
      <CardHeader title="Heatmap" subtitle={`Top ${top.length} industries`} />
      <div className="p-4 overflow-x-auto">
        <table style={{ borderCollapse: 'collapse' }}>
          <thead>
            <tr>
              <th style={{ width: 120, textAlign: 'left', fontSize: 9, padding: '2px 6px' }}></th>
              {top.map(ind => (
                <th key={ind}
                  style={{
                    fontSize: 9, textAlign: 'center', padding: '2px 4px',
                    color: '#666', fontFamily: 'JetBrains Mono', fontWeight: 400,
                    maxWidth: 52, overflow: 'hidden', whiteSpace: 'nowrap',
                  }}>
                  <div style={{ writingMode: 'vertical-rl', transform: 'rotate(180deg)', height: 60 }}>
                    {short(ind)}
                  </div>
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {top.map(rowInd => (
              <tr key={rowInd}>
                <td style={{
                  fontSize: 9, fontFamily: 'JetBrains Mono', color: '#666',
                  padding: '2px 6px', maxWidth: 120, overflow: 'hidden', whiteSpace: 'nowrap',
                }}>
                  {short(rowInd)}
                </td>
                {top.map(colInd => {
                  const v = rowInd === colInd ? 1.0 : lookup.get(`${rowInd}||${colInd}`)
                  return (
                    <td key={colInd}
                      title={`${rowInd} ↔ ${colInd}: ${v?.toFixed(3) ?? 'n/a'}`}
                      style={{
                        background: cellColor(v),
                        width: 36, height: 24,
                        border: '1px solid #111',
                        textAlign: 'center',
                        fontSize: 8,
                        color: '#888',
                        fontFamily: 'JetBrains Mono',
                      }}>
                      {v != null && v !== 1.0 ? v.toFixed(2) : ''}
                    </td>
                  )
                })}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </Card>
  )
}

// ── Main ──────────────────────────────────────────────────────────────────────
export default function CorrelationMatrix() {
  const { data, error, isLoading, mutate } = useSWR('correlations', fetchCorrelations, {
    revalidateOnFocus: false,
  })

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="font-mono text-lg font-semibold">Industry Correlations</h1>
          <p className="text-muted text-sm mt-0.5">
            {data ? `${data.pairs.length} pairs · ${data.date}` : 'Cross-sector correlation matrix'}
          </p>
        </div>
        <Button onClick={() => mutate()} loading={isLoading} variant="ghost">
          <RefreshCw size={13} />
          Refresh
        </Button>
      </div>

      {error && <ErrorBox message={error.message} />}

      {isLoading && (
        <div className="flex items-center justify-center h-48">
          <Spinner size={24} />
        </div>
      )}

      {data && (
        <>
          {/* Heatmap */}
          <MiniHeatmap industries={data.industries} pairs={data.pairs} />

          {/* Most / least correlated */}
          <div className="grid grid-cols-2 gap-5">
            <PairsTable pairs={data.top_correlated}   title="Most Correlated" />
            <PairsTable pairs={data.least_correlated} title="Least Correlated" />
          </div>
        </>
      )}

      {!isLoading && !data && !error && (
        <div className="text-center py-12 text-muted font-mono text-sm">
          Click Refresh to compute correlations
        </div>
      )}
    </div>
  )
}
