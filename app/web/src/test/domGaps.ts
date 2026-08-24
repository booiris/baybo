// jsdom stops short of a few DOM APIs the dashboard depends on. None is exotic
// in a browser — they are simply unimplemented — so the suites that need them
// install the missing halves rather than let a component branch around a test
// environment.

/** jsdom ships Pointer Events but not the capture API, so any handler calling
 * `setPointerCapture` throws mid-dispatch and surfaces as an unhandled error
 * rather than a test failure. Capture only decides which element later moves
 * retarget to; no-ops are faithful enough where there is no real pointer. */
export function installPointerCapture(): void {
  const proto = Element.prototype as unknown as Record<string, unknown>;
  proto.setPointerCapture ??= () => {};
  proto.releasePointerCapture ??= () => {};
  proto.hasPointerCapture ??= () => false;
}

/** jsdom has no blob URL store, so `URL.createObjectURL` / `revokeObjectURL` are
 * absent entirely and an unmount that revokes throws. Installed for the whole
 * file and deliberately NOT restored: Testing Library's `cleanup()` unmounts in
 * a setup-file `afterEach` that runs AFTER the suite's own, so tearing these
 * down early puts the gap back exactly when React needs them. */
export function installObjectUrls(url: string): void {
  Object.assign(URL, {
    createObjectURL: () => url,
    revokeObjectURL: () => {},
  });
}

/** jsdom runs no layout, so every element reports `scrollHeight` and
 * `clientHeight` as a hard 0 and swallows writes to `scrollTop`. A pane that
 * sticks to its own bottom is then untestable in both directions at once: it
 * can neither be scrolled off the edge nor be caught putting itself back.
 *
 * Installs one box for every element — the two numbers a test needs, and a
 * `scrollTop` that remembers what was written to it. Not restored, for the
 * reason `installObjectUrls` is not. `scrollTo` comes with it, since a smooth
 * jump goes through that rather than the property. */
export function installScrollBox(box: { scrollHeight: number; clientHeight: number }): void {
  const tops = new WeakMap<Element, number>();
  const proto = Element.prototype;
  Object.defineProperty(proto, 'scrollTop', {
    configurable: true,
    get(this: Element) {
      return tops.get(this) ?? 0;
    },
    set(this: Element, value: number) {
      tops.set(this, value);
    },
  });
  for (const [name, value] of [
    ['scrollHeight', box.scrollHeight],
    ['clientHeight', box.clientHeight],
  ] as const) {
    Object.defineProperty(proto, name, { configurable: true, get: () => value });
  }
  proto.scrollTo = function scrollTo(this: Element, options?: ScrollToOptions | number) {
    tops.set(this, typeof options === 'number' ? options : (options?.top ?? 0));
  } as Element['scrollTo'];
}
