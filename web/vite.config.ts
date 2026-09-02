import { defineConfig } from "vite";
import preact from "@preact/preset-vite";

// The bundle is embedded in the gateway binary and served from `/app`, so every
// asset URL has to be relative to that prefix rather than to the site root.
export default defineConfig({
  base: "/app/",
  plugins: [preact()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // One JS and one CSS file keeps the embedding (and the CSP) simple.
    assetsInlineLimit: 4096,
  },
  server: {
    port: 5173,
    // `pnpm dev` talks to a gateway running locally; the bundle itself is
    // origin-agnostic, this is only for convenience during development.
    proxy: {
      "/sessions": "http://127.0.0.1:48100",
      "/devices": "http://127.0.0.1:48100",
      "/pairing": "http://127.0.0.1:48100",
      "/machines": "http://127.0.0.1:48100",
      "/agents": "http://127.0.0.1:48100",
      "/health": "http://127.0.0.1:48100",
      "/remote": { target: "ws://127.0.0.1:48100", ws: true },
    },
  },
});
