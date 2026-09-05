import "@testing-library/jest-dom/vitest";
import { afterEach, vi } from "vitest";
import { cleanup } from "@testing-library/react";

// Registered for every suite via vitest `setupFiles`. jest-dom extends `expect`
// with DOM matchers; `cleanup` unmounts what a render() test left behind. Both
// are inert for the pure-reducer suites that never render.
afterEach(() => {
  cleanup();
});

// jsdom has no ResizeObserver, and `TableBlock` builds one in a mount effect.
// Since `MarkdownFallback` exists that throw is CAUGHT, so a markdown table
// silently renders as `.md-failed` raw source and the assertion around it still
// passes — stub it globally so a table stays a table wherever markdown mounts.
vi.stubGlobal(
  "ResizeObserver",
  class {
    observe(): void {}
    unobserve(): void {}
    disconnect(): void {}
  },
);
