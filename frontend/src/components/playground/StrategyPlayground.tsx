'use client'

import { useState } from 'react'
import useSWR from 'swr'
import { Play, Wand2, ChevronRight } from 'lucide-react'
import { fetchPresets, runPreset, fetchPicks, parseStrategy } from '@/lib/api'
import {
  Card, CardHeader, Button, Badge, Spinner, Empty, ErrorBox, ScoreBar,
} from '@/components/ui'
import SignalModal from '@/components/SignalModal'
import type { PresetStrategy, PicksResponse, SignalScore } from '@/lib/types'
import clsx from 'clsx'

// ── Preset card ───────────────────────────────────────────────────────────────
function PresetCard({
  preset, selected, onClick,
}: { preset: PresetStrategy; selected: boolean; onClick: () => void }) {
  return (
    <button
      onClick={onClick}
      className={clsx(
        'text-left p-3 rounded-lg border transition-all',
        selected
          ? 'border-accent/50 bg-accent/10'
          : 'border-border bg-panel hover:border-[#333] hover:bg-white/5',
      )}
    >
      <div className="flex items-center gap-2 mb-1">
        <span className="text-lg">{preset.icon}</span>
        <span className="font-mono text-sm font-medium text-[#E0E0E0] leading-tight">{preset.name}</span>
      </div>
      <p className="text-[11px] text-muted leading-snug">{preset.description}</p>
    </button>
  )
}

// ── Results table ─────────────────────────────────────────────────────────────
function ResultsTable({
  picks, onSelect,
}: { picks: SignalScore[]; onSelect: (t: string) => void }) {
  if (picks.length === 0) return <Empty message="No picks returned" />
  return (
    <table>
      <thead>
        <tr>
          <th className="w-8">#</th>
          <th>Ticker</th>
          <th>Industry</th>
          <th>Score</th>
          <th>Mom</th>
          <th>Fund</th>
          <th>Insd</th>
          <th>Sntm</th>
          <th></th>
        </tr>
      </thead>
      <tbody>
        {picks.map(p => (
          <tr key={p.ticker} className="cursor-pointer" onClick={() => onSelect(p.ticker)}>
            <td className="text-muted">{p.rank}</td>
            <td className="text-accent font-semibold">{p.ticker}</td>
            <td className="text-muted max-w-[160px] truncate">{p.industry}</td>
            <td className="w-32"><ScoreBar value={p.composite} /></td>
            <td className="text-xs">{p.momentum_contrib.toFixed(0)}</td>
            <td className="text-xs">{p.fundamental_contrib.toFixed(0)}</td>
            <td className="text-xs">{p.insider_contrib.toFixed(0)}</td>
            <td className="text-xs">{p.sentiment_contrib.toFixed(0)}</td>
            <td><ChevronRight size={13} className="text-muted" /></td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}

// ── Main component ────────────────────────────────────────────────────────────
export default function StrategyPlayground() {
  const [selectedPreset, setSelectedPreset] = useState<string | null>(null)
  const [nlInput, setNlInput] = useState('')
  const [mode, setMode] = useState<'preset' | 'custom'>('preset')
  const [market, setMarket] = useState('US')
  const [topN, setTopN] = useState(15)
  const [running, setRunning] = useState(false)
  const [results, setResults] = useState<PicksResponse | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [selectedTicker, setSelectedTicker] = useState<string | null>(null)

  const { data: presets, isLoading: loadingPresets } = useSWR('presets', fetchPresets, {
    revalidateOnFocus: false,
  })

  const run = async () => {
    setError(null)
    setRunning(true)
    try {
      let res: PicksResponse
      if (mode === 'preset' && selectedPreset) {
        res = await runPreset(selectedPreset, topN)
      } else if (mode === 'custom' && nlInput.trim()) {
        const spec = await parseStrategy(nlInput.trim())
        res = await fetchPicks({ strategy: spec, market, top_n: topN })
      } else {
        setError('Select a preset or enter a strategy description')
        return
      }
      setResults(res)
    } catch (e: unknown) {
      setError((e as Error).message)
    } finally {
      setRunning(false)
    }
  }

  return (
    <div className="space-y-6">
      {/* Header */}
      <div>
        <h1 className="font-mono text-lg font-semibold">Strategy Playground</h1>
        <p className="text-muted text-sm mt-0.5">Pick a preset or describe your strategy in plain English</p>
      </div>

      <div className="grid grid-cols-3 gap-5">
        {/* Left panel — strategy selector */}
        <div className="col-span-1 space-y-4">
          {/* Mode tabs */}
          <div className="flex rounded-lg border border-border overflow-hidden">
            {(['preset', 'custom'] as const).map(m => (
              <button
                key={m}
                onClick={() => setMode(m)}
                className={clsx(
                  'flex-1 py-2 text-xs font-mono font-medium uppercase tracking-wider transition-colors',
                  mode === m ? 'bg-accent/15 text-accent' : 'text-muted hover:text-[#E0E0E0]',
                )}
              >
                {m === 'preset' ? '12 Presets' : 'AI Custom'}
              </button>
            ))}
          </div>

          {mode === 'preset' && (
            <>
              {loadingPresets && <div className="flex justify-center py-6"><Spinner /></div>}
              {presets && (
                <div className="space-y-2 max-h-[60vh] overflow-y-auto pr-1">
                  {presets.map(p => (
                    <PresetCard
                      key={p.id}
                      preset={p}
                      selected={selectedPreset === p.id}
                      onClick={() => setSelectedPreset(p.id)}
                    />
                  ))}
                </div>
              )}
            </>
          )}

          {mode === 'custom' && (
            <div className="space-y-3">
              <label className="text-xs text-muted font-mono">Strategy description</label>
              <textarea
                value={nlInput}
                onChange={e => setNlInput(e.target.value)}
                rows={6}
                placeholder="e.g. Buy high-momentum large cap tech stocks with positive insider buying and strong revenue growth…"
                className="w-full text-sm resize-none"
              />
              <div className="flex items-center gap-1 text-[10px] text-muted font-mono">
                <Wand2 size={11} />
                Parsed by Gemini 2.5 Flash
              </div>
            </div>
          )}

          {/* Options */}
          <div className="space-y-3 pt-2 border-t border-border">
            <div className="flex items-center gap-3">
              <label className="text-xs text-muted font-mono w-16">Market</label>
              <select
                value={market}
                onChange={e => setMarket(e.target.value)}
                className="flex-1 text-xs"
              >
                <option value="US">US</option>
                <option value="IN">India (NSE)</option>
                <option value="ALL">All</option>
              </select>
            </div>
            <div className="flex items-center gap-3">
              <label className="text-xs text-muted font-mono w-16">Top N</label>
              <input
                type="number"
                min={1}
                max={50}
                value={topN}
                onChange={e => setTopN(Number(e.target.value))}
                className="flex-1 text-xs"
              />
            </div>
          </div>

          <Button
            className="w-full justify-center"
            onClick={run}
            loading={running}
          >
            <Play size={13} />
            Run Strategy
          </Button>

          {error && <ErrorBox message={error} />}
        </div>

        {/* Right panel — results */}
        <div className="col-span-2">
          <Card className="h-full">
            <CardHeader
              title="Results"
              subtitle={results ? `${results.date} · ${results.picks.length} picks · regime: ${results.regime}` : undefined}
              action={results && (
                <Badge color={results.regime === 'risk_on' ? 'green' : 'red'}>
                  {results.regime}
                </Badge>
              )}
            />
            <div>
              {!results && !running && (
                <Empty message="Run a strategy to see picks" />
              )}
              {running && (
                <div className="flex items-center justify-center h-40">
                  <Spinner size={22} />
                </div>
              )}
              {results && !running && (
                <ResultsTable picks={results.picks} onSelect={setSelectedTicker} />
              )}
            </div>
          </Card>
        </div>
      </div>

      <SignalModal ticker={selectedTicker} onClose={() => setSelectedTicker(null)} />
    </div>
  )
}
