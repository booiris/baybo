import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

// Vitest keeps its own config so the production `vite.config.ts` (file:// base +
// build target) stays free of test concerns. `jsdom` is NOT optional: the
// reducer modules transitively import `bridge.ts`, which at MODULE SCOPE reads
// `window.webkit` and registers window listeners, so `node` throws on import.
// Most suites exercise pure reducers; the one render test (WorkBlock.test.tsx)
// mounts that small presentational card — no scrollHeight/follow/pin, so it does
// NOT hit the reason <Transcript> stays unrendered (see
// app/ios/docs/testing.md "web/"). `setup.ts` wires the jest-dom matchers and
// per-test unmount.
export default defineConfig({
  plugins: [react()],
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.ts", "src/**/*.test.tsx"],
    setupFiles: ["./src/test/setup.ts"],
  },
});
