'use client'

import clsx from 'clsx'
import { Loader2 } from 'lucide-react'
import type { ReactNode, ButtonHTMLAttributes } from 'react'

// ── Card ──────────────────────────────────────────────────────────────────────
export function Card({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <div className={clsx('bg-panel border border-border rounded-lg', className)}>
      {children}
    </div>
  )
}

// ── CardHeader ────────────────────────────────────────────────────────────────
export function CardHeader({ title, subtitle, action }: {
  title: string; subtitle?: string; action?: ReactNode
}) {
  return (
    <div className="flex items-center justify-between px-4 py-3 border-b border-border">
      <div>
        <h2 className="font-mono text-sm font-semibold text-[#E0E0E0] uppercase tracking-wider">
          {title}
        </h2>
        {subtitle && <p className="text-[11px] text-muted mt-0.5">{subtitle}</p>}
      </div>
      {action && <div>{action}</div>}
    </div>
  )
}

// ── Spinner ───────────────────────────────────────────────────────────────────
export function Spinner({ size = 16 }: { size?: number }) {
  return <Loader2 size={size} className="animate-spin text-accent" />
}

// ── Button ────────────────────────────────────────────────────────────────────
interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: 'primary' | 'ghost' | 'danger'
  loading?: boolean
}
export function Button({ children, variant = 'primary', loading, className, disabled, ...props }: ButtonProps) {
  return (
    <button
      {...props}
      disabled={disabled || loading}
      className={clsx(
        'inline-flex items-center gap-2 px-3 py-1.5 rounded text-sm font-mono font-medium transition-all',
        variant === 'primary' && 'bg-accent/15 text-accent border border-accent/30 hover:bg-accent/25',
        variant === 'ghost'   && 'text-muted border border-border hover:text-[#E0E0E0] hover:border-[#333]',
        variant === 'danger'  && 'text-negative border border-negative/30 hover:bg-negative/10',
        (disabled || loading) && 'opacity-40 cursor-not-allowed',
        className,
      )}
    >
      {loading && <Spinner size={13} />}
      {children}
    </button>
  )
}

// ── Badge ─────────────────────────────────────────────────────────────────────
export function Badge({ children, color = 'default' }: {
  children: ReactNode; color?: 'green' | 'red' | 'yellow' | 'cyan' | 'default'
}) {
  return (
    <span className={clsx(
      'inline-block px-1.5 py-0.5 rounded text-[10px] font-mono font-medium uppercase tracking-wider',
      color === 'green'   && 'bg-positive/15 text-positive',
      color === 'red'     && 'bg-negative/15 text-negative',
      color === 'yellow'  && 'bg-warning/15 text-warning',
      color === 'cyan'    && 'bg-accent/15 text-accent',
      color === 'default' && 'bg-white/5 text-muted',
    )}>
      {children}
    </span>
  )
}

// ── StatCard ──────────────────────────────────────────────────────────────────
export function StatCard({ label, value, sub, color }: {
  label: string; value: string | number; sub?: string; color?: 'green' | 'red' | 'cyan'
}) {
  return (
    <div className="bg-panel border border-border rounded-lg px-4 py-3">
      <div className="text-[10px] text-muted uppercase tracking-wider font-mono mb-1">{label}</div>
      <div className={clsx('font-mono font-semibold text-xl',
        color === 'green' && 'text-positive',
        color === 'red'   && 'text-negative',
        color === 'cyan'  && 'text-accent',
        !color && 'text-[#E0E0E0]',
      )}>{value}</div>
      {sub && <div className="text-[10px] text-muted mt-0.5">{sub}</div>}
    </div>
  )
}

// ── ScoreBar ──────────────────────────────────────────────────────────────────
export function ScoreBar({ value, max = 100 }: { value: number; max?: number }) {
  const pct = Math.max(0, Math.min(100, (value / max) * 100))
  const color = pct >= 70 ? '#00C853' : pct >= 40 ? '#00E5FF' : '#FF1744'
  return (
    <div className="flex items-center gap-2">
      <div className="flex-1 h-1.5 bg-white/5 rounded-full overflow-hidden">
        <div style={{ width: `${pct}%`, background: color }} className="h-full rounded-full transition-all" />
      </div>
      <span className="font-mono text-xs w-8 text-right" style={{ color }}>{value.toFixed(0)}</span>
    </div>
  )
}

// ── Pct helper ────────────────────────────────────────────────────────────────
export function Pct({ value, decimals = 2 }: { value: number | null | undefined; decimals?: number }) {
  if (value == null) return <span className="text-muted">—</span>
  const positive = value >= 0
  return (
    <span className={positive ? 'text-positive' : 'text-negative'}>
      {positive ? '+' : ''}{value.toFixed(decimals)}%
    </span>
  )
}

// ── Empty state ───────────────────────────────────────────────────────────────
export function Empty({ message = 'No data' }: { message?: string }) {
  return (
    <div className="flex items-center justify-center h-32 text-muted text-sm font-mono">
      {message}
    </div>
  )
}

// ── Error state ───────────────────────────────────────────────────────────────
export function ErrorBox({ message }: { message: string }) {
  return (
    <div className="bg-negative/10 border border-negative/30 rounded p-3 text-sm text-negative font-mono">
      {message}
    </div>
  )
}
