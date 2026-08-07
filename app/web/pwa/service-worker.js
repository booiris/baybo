/**
 * Baybo dashboard service worker.
 *
 * Not part of the app bundle. `pwa/plugin.ts` reads this file at the end of a
 * production build, substitutes `__BUILD_ID__` and `__PRECACHE__` from the real
 * `dist/` tree, and writes the result to `dist/sw.js`. There is no service
 * worker in `vite dev` — `src/pwa/registerSW.ts` registers only in a production
 * bundle, so HMR never fights a cache.
 *
 * The contract with the gateway (`crates/gateway/src/api/webui.rs`):
 *   - `/assets/*` is content-hashed and immutable; a missing one is a 404, not
 *     an SPA fallback, so a stale precache can never be papered over with HTML.
 *   - `/sw.js` and `/index.html` are served `no-cache`, which is what lets the
 *     browser notice a new worker on the next navigation.
 *   - `/v1/*`, `/healthz`, `/readyz` are live data and are never touched here.
 */

const BUILD_ID = '__BUILD_ID__';
/** Absolute paths, injected at build time. */
const PRECACHE_URLS = '__PRECACHE__';

/** Rotates with the bundle, so activating a new worker drops the old one whole
 *  — precached shell and runtime-filled assets together, never half of each. */
const APP_CACHE = `baybo-app-${BUILD_ID}`;
/** Deliberately NOT build-scoped: the webfonts are the same bytes across
 *  deploys and re-downloading ~300 KB on every gateway upgrade is waste. */
const FONT_CACHE = 'baybo-fonts-v1';
const CACHE_PREFIX = 'baybo-';

/** The precached document every navigation falls back to. `/` and every SPA
 *  deep link resolve to these bytes, so the fallback is keyed on the file, not
 *  on the request URL. */
const APP_SHELL = '/index.html';

const FONT_ORIGINS = ['https://fonts.googleapis.com', 'https://fonts.gstatic.com'];

/** Live gateway surfaces. Never cached, never fallen back to — a cached
 *  `/v1/*` answer would be a stale (and bearer-scoped) lie. */
const LIVE_PATH = /^\/(?:v1(?:\/|$)|healthz$|readyz$)/;

/** How long a navigation waits on the gateway before the cached shell wins.
 *  Bounds the offline-but-not-refused case (captive portal, dropped VPN),
 *  where the request would otherwise hang until the browser gives up. */
const NAVIGATION_TIMEOUT_MS = 4000;

self.addEventListener('install', (event) => {
  event.waitUntil(
    caches.open(APP_CACHE).then((cache) =>
      // `cache: 'reload'` bypasses the HTTP cache, so the precache can never be
      // seeded from a stale copy of index.html the browser still holds.
      cache.addAll(PRECACHE_URLS.map((url) => new Request(url, { cache: 'reload' }))),
    ),
  );
  // No skipWaiting() here on purpose: swapping the bundle under a live page
  // would break a streaming turn. The new worker waits until the user accepts
  // the reload prompt, which posts SKIP_WAITING below. A FIRST install has no
  // controller to wait for and activates immediately either way.
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    (async () => {
      const names = await caches.keys();
      await Promise.all(
        names
          .filter(
            (name) =>
              name.startsWith(CACHE_PREFIX) && name !== APP_CACHE && name !== FONT_CACHE,
          )
          .map((name) => caches.delete(name)),
      );
      await self.clients.claim();
    })(),
  );
});

self.addEventListener('message', (event) => {
  if (event.data && event.data.type === 'SKIP_WAITING') {
    void self.skipWaiting();
  }
});

self.addEventListener('fetch', (event) => {
  const request = event.request;
  // A Range request served whole from the Cache API is a broken response; let
  // the network answer those.
  if (request.method !== 'GET' || request.headers.has('range')) return;

  let url;
  try {
    url = new URL(request.url);
  } catch {
    return;
  }

  if (FONT_ORIGINS.includes(url.origin)) {
    event.respondWith(cacheFirst(request, FONT_CACHE));
    return;
  }
  // Any other third party (none today) stays entirely ours-to-not-touch.
  if (url.origin !== self.location.origin) return;
  if (LIVE_PATH.test(url.pathname)) return;

  if (request.mode === 'navigate') {
    event.respondWith(shellNetworkFirst(request));
    return;
  }
  event.respondWith(cacheFirst(request, APP_CACHE));
});

/**
 * Documents are network-first: the gateway is usually a local process, the
 * bundle it serves is the source of truth, and serving a cached shell while a
 * newer one is one hop away buys a stale UI for nothing. The cache is the
 * offline floor, not the fast path.
 *
 * The deadline is a race rather than an `AbortController`, because arming one
 * means passing an init to `fetch()` — and a navigation request re-fetched with
 * a non-empty init has its `navigate` mode rewritten out from under it. The
 * losing request is left to finish into the HTTP cache; only its result is
 * dropped.
 */
async function shellNetworkFirst(request) {
  const network = fetch(request);
  // The race may discard this promise; a later rejection must not surface as an
  // unhandled one.
  const settled = network.catch(() => null);

  const cache = await caches.open(APP_CACHE);
  const shell = await cache.match(APP_SHELL);
  if (!shell) return network;

  const deadline = new Promise((resolve) => setTimeout(() => resolve(null), NAVIGATION_TIMEOUT_MS));
  return (await Promise.race([settled, deadline])) ?? shell;
}

/**
 * Sub-resources are cache-first. Everything under `/assets/` is content-hashed,
 * so a hit can never be stale; anything else (icons, the manifest) rotates with
 * `APP_CACHE` on the next activation.
 */
async function cacheFirst(request, cacheName) {
  const cache = await caches.open(cacheName);
  const cached = await cache.match(request);
  if (cached) return cached;

  const response = await fetch(request);
  if (response.ok) {
    // `put` consumes a body, so store the clone and hand the original back.
    // A quota failure must not fail the request the page is waiting on.
    void cache.put(request, response.clone()).catch(() => {});
  }
  return response;
}
