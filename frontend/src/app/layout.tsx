import type { Metadata } from 'next'
import './globals.css'
import Sidebar from '@/components/Sidebar'

export const metadata: Metadata = {
  title: 'Quant Edge',
  description: 'Algorithmic portfolio intelligence',
}

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en" className="dark">
      <body className="bg-bg text-[#E0E0E0] min-h-screen">
        <Sidebar />
        {/* Main content — offset by sidebar width */}
        <main className="ml-56 min-h-screen p-6">
          {children}
        </main>
      </body>
    </html>
  )
}
