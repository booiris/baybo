import '@testing-library/jest-dom/vitest';
import { afterEach } from 'vitest';
import { cleanup } from '@testing-library/react';

// Registered for every suite via vitest `setupFiles`. jest-dom extends
// `expect` with DOM matchers; `cleanup` unmounts anything a render() test left
// behind. Both are inert for the pure-reducer suites that never render.
// jsdom implements no layout, so it omits `scrollIntoView` entirely — a
// component that keeps a thread pinned to the bottom would throw rather
// than simply not scroll. Stubbed once here rather than per suite, because
// it is the environment that is incomplete, not the component.
Element.prototype.scrollIntoView = () => {};

afterEach(() => {
  cleanup();
});
