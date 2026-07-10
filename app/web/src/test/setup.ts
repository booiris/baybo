// Vitest setup: wires jest-dom's matchers (toBeDisabled, toHaveTextContent, …)
// into vitest's `expect`, both the runtime extension and the TS types.
import '@testing-library/jest-dom/vitest';

import { afterEach } from 'vitest';
import { cleanup } from '@testing-library/react';

// `@testing-library/react`'s own auto-cleanup only self-registers when it
// finds a global `afterEach` (the jest-compat convention); this project
// doesn't turn on vitest's `test.globals`, so wire it explicitly — without
// this, a suite with more than one `render()` leaks prior trees into
// `document.body` and later queries see duplicate nodes.
afterEach(() => {
  cleanup();
});
