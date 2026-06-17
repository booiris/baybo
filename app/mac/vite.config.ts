import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';

// Tauri serves the webview from this dev origin (must match
// `build.devUrl` in tauri.conf.json and the dev CORS origin the embed
// allowlists, docs/mac-app.md §2). No backend proxy: the webview talks
// directly to the in-process gateway on its loopback ephemeral port,
// whose base URL arrives via the `get_connection` Tauri command.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    outDir: 'dist',
    target: 'safari15',
  },
});
