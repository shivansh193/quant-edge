import type { Config } from 'tailwindcss'

const config: Config = {
  content: [
    './src/pages/**/*.{js,ts,jsx,tsx,mdx}',
    './src/components/**/*.{js,ts,jsx,tsx,mdx}',
    './src/app/**/*.{js,ts,jsx,tsx,mdx}',
  ],
  theme: {
    extend: {
      colors: {
        bg:       '#0D0D0D',
        panel:    '#141414',
        border:   '#1E1E1E',
        accent:   '#00E5FF',
        'accent-dim': '#00B8CC',
        positive: '#00C853',
        negative: '#FF1744',
        warning:  '#FFD600',
        muted:    '#888888',
      },
      fontFamily: {
        mono: ['JetBrains Mono', 'Menlo', 'Monaco', 'monospace'],
        sans: ['Inter', 'system-ui', 'sans-serif'],
      },
      borderColor: {
        DEFAULT: '#1E1E1E',
      },
    },
  },
  plugins: [],
}

export default config
