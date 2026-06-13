import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';

// Dev-only: forward the JSON API to the running bench-web binary so the
// browser only ever talks to the Vite origin (no CORS). `/assets/...`
// stays in Vite's module graph. Override the target with BENCH_WEB_URL.
const target = process.env.BENCH_WEB_URL ?? 'http://127.0.0.1:7000';

export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    proxy: {
      '/api': { target, changeOrigin: true },
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
});
