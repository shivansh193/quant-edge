/** @type {import('next').NextConfig} */
const nextConfig = {
  // Static export — output goes to `out/`, served by the Rust Axum server in production.
  // In development, run `npm run dev` (port 3000) alongside the Rust server (port 8080).
  output: 'export',
  trailingSlash: true,
  images: { unoptimized: true },
}

export default nextConfig
