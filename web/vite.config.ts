import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';

// Dev-only: the admin TCP listener holds both `/v1/*` and the embedded
// React bundle. When iterating on the UI via `npm run dev`, forward
// every privileged path to the gateway so the browser only talks to
// the Vite origin — no CORS allow-list, no baseUrl override. Override
// the target with `AURA_GATEWAY_URL` when running against a non-local
// gateway. `/assets/...` is intentionally NOT proxied: those belong to
// Vite's module graph.
const gatewayTarget = process.env.AURA_GATEWAY_URL ?? 'http://127.0.0.1:8888';

export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    proxy: {
      '/v1': gatewayTarget,
      '/healthz': gatewayTarget,
      '/readyz': gatewayTarget,
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
});
