'use client'

import Link from 'next/link'
import { usePathname } from 'next/navigation'
import clsx from 'clsx'
import {
  Sun,
  Beaker,
  ChartLine,
  Wallet,
  Globe,
  Activity,
  Zap,
} from 'lucide-react'

const NAV = [
  { href: '/',             icon: Sun,       label: 'Morning',     short: 'AM'  },
  { href: '/playground/',  icon: Beaker,    label: 'Playground',  short: 'PG'  },
  { href: '/backtest/',    icon: ChartLine, label: 'Backtest',    short: 'BT'  },
  { href: '/paper/',       icon: Wallet,    label: 'Paper',       short: 'PP'  },
  { href: '/universe/',    icon: Globe,     label: 'Universe',    short: 'UN'  },
  { href: '/correlations/',icon: Activity,  label: 'Correlations',short: 'CO'  },
]

export default function Sidebar() {
  const path = usePathname()

  return (
    <aside className="fixed inset-y-0 left-0 w-56 bg-panel border-r border-border flex flex-col z-40">
      {/* Logo */}
      <div className="flex items-center gap-2 px-4 h-14 border-b border-border shrink-0">
        <Zap className="text-accent" size={18} />
        <span className="font-mono font-semibold text-sm tracking-wider text-accent">
          QUANT EDGE
        </span>
      </div>

      {/* Nav */}
      <nav className="flex-1 overflow-y-auto py-3 space-y-0.5 px-2">
        {NAV.map(({ href, icon: Icon, label }) => {
          const active = path === href || (href !== '/' && path.startsWith(href.slice(0, -1)))
          return (
            <Link
              key={href}
              href={href}
              className={clsx(
                'flex items-center gap-3 px-3 py-2 rounded text-sm transition-colors',
                active
                  ? 'bg-accent/10 text-accent border border-accent/20'
                  : 'text-muted hover:text-[#E0E0E0] hover:bg-white/5'
              )}
            >
              <Icon size={15} />
              <span>{label}</span>
            </Link>
          )
        })}
      </nav>

      {/* Footer */}
      <div className="px-4 py-3 border-t border-border text-[10px] text-muted/60 font-mono">
        <div>Quant Edge v0.1</div>
        <div className="mt-0.5 opacity-60">Rust + Next.js</div>
      </div>
    </aside>
  )
}
