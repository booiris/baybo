import { describe, expect, it } from 'vitest';
import { pwaBlocker, type PwaEnvironment } from './availability';

const LIVE: PwaEnvironment = {
  isProductionBuild: true,
  isSecureContext: true,
  hasServiceWorkerApi: true,
};

describe('pwaBlocker', () => {
  it('clears a production page on a secure origin', () => {
    expect(pwaBlocker(LIVE)).toBeNull();
  });

  it('says nothing about a dev build', () => {
    expect(pwaBlocker({ ...LIVE, isProductionBuild: false })).toBe('dev-build');
  });

  it('blames the origin, not the browser, when both look wrong', () => {
    // The regression this exists for: `ServiceWorkerContainer` is
    // `[SecureContext]`, so an insecure origin ALSO has no
    // `navigator.serviceWorker`. Checking the API first reported "unsupported"
    // — a dead end — for every LAN-IP visit.
    expect(pwaBlocker({ ...LIVE, isSecureContext: false, hasServiceWorkerApi: false })).toBe(
      'insecure-origin',
    );
  });

  it('reports an unsupported browser only on a secure origin', () => {
    expect(pwaBlocker({ ...LIVE, hasServiceWorkerApi: false })).toBe('unsupported');
  });

  it('stays quiet in dev even on an insecure origin', () => {
    expect(
      pwaBlocker({ isProductionBuild: false, isSecureContext: false, hasServiceWorkerApi: false }),
    ).toBe('dev-build');
  });
});
