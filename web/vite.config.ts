import { svelte } from '@sveltejs/vite-plugin-svelte';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vitest/config';

// The daemon embeds `dist/` (crates/ship-daemon/build.rs) and serves it
// same-origin with a strict CSP: no inline scripts or styles, no remote
// URLs. `/assets/*` is served as immutable, so every file there is hashed.
export default defineConfig({
  plugins: [tailwindcss(), svelte()],
  base: '/',
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    assetsDir: 'assets',
    // Inlined assets would become data: URLs or inline code; keep files.
    assetsInlineLimit: 0,
    modulePreload: { polyfill: false },
    sourcemap: false,
  },
  server: {
    // `npm run dev` against a daemon on its default 127.0.0.1 port.
    proxy: { '/api': 'http://127.0.0.1:7180', '/healthz': 'http://127.0.0.1:7180' },
  },
  test: {
    include: ['src/**/*.test.ts'],
    environment: 'node',
  },
});
