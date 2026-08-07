import { pwaBlocker, readPwaEnvironment } from './availability';
import { InstallPrompt, type InstallPromptEvent } from './install';
import { ServiceWorkerUpdates } from './updates';

/** Root scope, so one worker covers `/`, `/assets/*`, and every SPA deep link. */
const SW_URL = '/sw.js';
const SW_SCOPE = '/';

/** A tab left open for days must still notice a gateway upgrade. */
const UPDATE_POLL_MS = 60 * 60 * 1000;
/** …but a dashboard tab is focused and blurred all day, and each check is a
 *  conditional GET of `/sw.js`. */
const UPDATE_ON_FOCUS_THROTTLE_MS = 5 * 60 * 1000;

export const serviceWorkerUpdates = new ServiceWorkerUpdates(() => {
  window.location.reload();
});
export const installPrompt = new InstallPrompt();

/**
 * Wires the PWA to the page. Safe to call unconditionally: everything below
 * degrades to a no-op where the platform has nothing to offer (no worker in
 * dev, no worker off a secure origin, no install event outside Chromium).
 */
export function registerPwa(): void {
  window.addEventListener('beforeinstallprompt', (event) => {
    // Suppress Chrome's own mini-infobar; the rail owns the affordance.
    event.preventDefault();
    installPrompt.offer(event as unknown as InstallPromptEvent);
  });
  window.addEventListener('appinstalled', () => {
    installPrompt.onInstalled();
  });
  void registerServiceWorker();
}

async function registerServiceWorker(): Promise<void> {
  const blocker = pwaBlocker(readPwaEnvironment());
  if (blocker === 'insecure-origin') {
    // The common shape of this: reaching the gateway at `http://<lan-ip>:8888`.
    // Nothing surfaces in the UI, so this line is the only signal there is.
    console.info(
      `[pwa] service worker skipped — ${window.location.origin} is not a secure context (needs https or localhost)`,
    );
    return;
  }
  if (blocker !== null) return;

  const container = navigator.serviceWorker;
  container.addEventListener('controllerchange', () => {
    serviceWorkerUpdates.onControllerChange();
  });

  let registration: ServiceWorkerRegistration;
  try {
    registration = await container.register(SW_URL, { scope: SW_SCOPE });
  } catch (error) {
    console.warn('[pwa] service worker registration failed', error);
    return;
  }

  // A worker that finished installing during an earlier visit is still waiting.
  if (registration.waiting) {
    serviceWorkerUpdates.offer(registration.waiting, container.controller !== null);
  }

  registration.addEventListener('updatefound', () => {
    const installing = registration.installing;
    if (!installing) return;
    // Latched here, not read inside the handler: whether this page was already
    // controlled is what separates an update from a first install, and a first
    // worker's `clients.claim()` sets `controller` on its way through.
    const pageWasControlled = container.controller !== null;
    installing.addEventListener('statechange', () => {
      if (installing.state === 'installed') {
        serviceWorkerUpdates.offer(installing, pageWasControlled);
      }
    });
  });

  watchForUpdates(registration);
}

function watchForUpdates(registration: ServiceWorkerRegistration): void {
  let lastCheck = Date.now();
  const check = (): void => {
    lastCheck = Date.now();
    registration.update().catch(() => {});
  };

  window.setInterval(check, UPDATE_POLL_MS);
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState !== 'visible') return;
    if (Date.now() - lastCheck < UPDATE_ON_FOCUS_THROTTLE_MS) return;
    check();
  });
}
