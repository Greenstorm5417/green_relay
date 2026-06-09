import type { NextConfig } from "next";

// Static export: `next build` emits a fully static site to `web-ui/out`
// (HTML/CSS/JS, no Node server) so the Rust service can serve it directly when
// the `web-ui` cargo feature is enabled.
const nextConfig: NextConfig = {
  output: "export",
  // next/image optimization needs a server; disable it for static export.
  images: { unoptimized: true },
  // Emit `route/index.html` so any static host resolves clean URLs without
  // server-side rewrites.
  trailingSlash: true,
};

export default nextConfig;
