/**
 * Why the PWA layer is inert. `null` from [`pwaBlocker`] means it is live.
 *
 * Nothing in the UI surfaces this — an insecure origin logs one `console.info`
 * and that is all. The rail carried a popover explaining the fix for a while;
 * it was cut as noise.
 */
export type PwaBlocker = 'dev-build' | 'insecure-origin' | 'unsupported';

export interface PwaEnvironment {
  readonly isProductionBuild: boolean;
  readonly isSecureContext: boolean;
  readonly hasServiceWorkerApi: boolean;
}

/**
 * The single place that decides whether a service worker can exist here — read
 * by both the registration and the notice that explains its absence, so the two
 * can never disagree about why nothing happened.
 */
export function pwaBlocker(env: PwaEnvironment): PwaBlocker | null {
  // Dev is served by Vite, which emits no worker. Nothing to explain.
  if (!env.isProductionBuild) return 'dev-build';
  // Ordered ahead of the API check deliberately. `ServiceWorkerContainer` is
  // `[SecureContext]`, so on `http://<lan-ip>` the browser does not expose
  // `navigator.serviceWorker` AT ALL — testing for the API first reports "this
  // browser can't" for what is really "this URL can't", which is the one
  // diagnosis a user cannot act on.
  if (!env.isSecureContext) return 'insecure-origin';
  if (!env.hasServiceWorkerApi) return 'unsupported';
  return null;
}

export function readPwaEnvironment(): PwaEnvironment {
  return {
    isProductionBuild: import.meta.env.PROD,
    isSecureContext: window.isSecureContext,
    hasServiceWorkerApi: 'serviceWorker' in navigator,
  };
}
