import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';

// Vitest keeps its own config so the production `vite.config.ts` (dev
// proxy + build output) stays free of test concerns. Most suites exercise
// pure reducers and never render — for those `jsdom` is just a safe import
// sandbox. The React Testing Library suites (queueStore / QueuePanel) do
// render into that jsdom; `setup.ts` wires their jest-dom matchers and
// per-test unmount.
export default defineConfig({
  plugins: [react()],
  test: {
    environment: 'jsdom',
    include: ['src/**/*.test.ts', 'src/**/*.test.tsx'],
    setupFiles: ['./src/test/setup.ts'],
  },
});
