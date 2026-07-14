import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

// Vitest keeps its own config so the production `vite.config.ts` (file:// base +
// build target) stays free of test concerns. The React plugin is here only so a
// suite can import `Transcript.tsx` / `WorkBlock.tsx` for their pure reducers
// without a JSX transform error — nothing renders. `jsdom` is NOT optional: those
// modules transitively import `bridge.ts`, which at MODULE SCOPE reads
// `window.webkit` and registers window listeners, so `node` throws on import.
export default defineConfig({
  plugins: [react()],
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.ts", "src/**/*.test.tsx"],
  },
});
