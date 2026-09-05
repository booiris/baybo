import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

// Vitest keeps its own config so the production `vite.config.ts` (file:// base +
// build target) stays free of test concerns. `jsdom` is NOT optional: the
// reducer modules transitively import `bridge.ts`, which at MODULE SCOPE reads
// `window.webkit` and registers window listeners, so `node` throws on import.
// Render suites use jsdom; transcript layout is faked because jsdom has none.
// `setup.ts` installs matchers and per-test unmounting.
export default defineConfig({
  plugins: [react()],
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.ts", "src/**/*.test.tsx"],
    setupFiles: ["./src/test/setup.ts"],
  },
});
