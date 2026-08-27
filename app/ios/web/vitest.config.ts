import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

// Vitest keeps its own config so the production `vite.config.ts` (file:// base +
// build target) stays free of test concerns. `jsdom` is NOT optional: the
// reducer modules transitively import `bridge.ts`, which at MODULE SCOPE reads
// `window.webkit` and registers window listeners, so `node` throws on import.
// Most suites exercise pure reducers. The ones that RENDER split in two: the
// components that need no layout are mounted as-is (WorkBlock.test.tsx, and the
// `issue/` suites, which mount the whole `<IssuePage>` — a card page is a
// document, so its DOM order and its wiring are testable without a box model),
// while <Transcript> is mounted under a fake layout of its own, because jsdom
// has none (transcriptScroll.test.tsx; see app/ios/docs/testing.md "web/").
// What no suite here can see is PAINT — colour, clipping, where a thing lands —
// which is `BayboUITests`' half. `setup.ts` wires the jest-dom matchers and
// per-test unmount.
export default defineConfig({
  plugins: [react()],
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.ts", "src/**/*.test.tsx"],
    setupFiles: ["./src/test/setup.ts"],
  },
});
