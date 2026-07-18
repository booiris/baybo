import "@testing-library/jest-dom/vitest";
import { afterEach } from "vitest";
import { cleanup } from "@testing-library/react";

// Registered for every suite via vitest `setupFiles`. jest-dom extends `expect`
// with DOM matchers; `cleanup` unmounts what a render() test left behind. Both
// are inert for the pure-reducer suites that never render.
afterEach(() => {
  cleanup();
});
