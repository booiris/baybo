import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';

// Vitest keeps its own config so the production `vite.config.ts` (dev
// proxy + build output) stays free of test concerns. The React plugin is
// here only so a test can import a component module for its JSX without a
// transform error — the suites themselves exercise pure reducers and
// never render, so `jsdom` is just a safe import sandbox.
export default defineConfig({
  plugins: [react()],
  test: {
    environment: 'jsdom',
    include: ['src/**/*.test.ts', 'src/**/*.test.tsx'],
  },
});
