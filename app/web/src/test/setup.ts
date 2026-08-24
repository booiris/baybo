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

// Same story: jsdom keeps no object-URL store, so `createObjectURL` is simply
// absent and a component that previews a picked file throws where a browser
// would show a thumbnail. The counter makes each URL distinct, which is what
// a test asserting "this one was revoked" needs.
let objectUrls = 0;
URL.createObjectURL = () => `blob:test/${String(++objectUrls)}`;
URL.revokeObjectURL = () => {};

afterEach(() => {
  cleanup();
});
