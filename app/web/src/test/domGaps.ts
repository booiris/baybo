// jsdom stops short of two DOM APIs the image viewer depends on. Neither is
// exotic in a browser — they are simply unimplemented — so suites that render
// the viewer install the missing halves rather than let the component branch
// around a test environment.

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
