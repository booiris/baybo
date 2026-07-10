import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';

// Vitest keeps its own config so the production `vite.config.ts` (dev
// proxy + build output) stays free of test concerns. The React plugin
// lets a test import a component module for its JSX without a transform
// error and, for the `.tsx` suites that actually mount a component (e.g.
// `AgentPicker.test.tsx`), render it via `@testing-library/react`; most
// suites still exercise pure reducers and never render, so `jsdom` is a
// safe import sandbox for those. `setupFiles` wires jest-dom's matchers
// into vitest's `expect` for every suite.
export default defineConfig({
  plugins: [react()],
  test: {
    environment: 'jsdom',
    include: ['src/**/*.test.ts', 'src/**/*.test.tsx'],
    setupFiles: ['./src/test/setup.ts'],
  },
});
