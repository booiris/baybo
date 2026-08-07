import { useState, useSyncExternalStore } from 'react';
import { RiCloseLine, RiRefreshLine } from 'react-icons/ri';
import { serviceWorkerUpdates } from './registerSW';

/**
 * Offers the reload that hands the page to an already-installed newer bundle.
 *
 * It asks instead of reloading on its own because a dashboard tab is usually
 * mid-something — a streaming turn, a half-typed message — and the worker
 * deliberately waits rather than swapping assets underneath it.
 *
 * Bottom-RIGHT, not centre: the chat composer owns the bottom of the reading
 * band at every width.
 */
export function PwaUpdateBanner() {
  const { updateReady } = useSyncExternalStore(
    serviceWorkerUpdates.subscribe,
    serviceWorkerUpdates.getSnapshot,
  );
  const [dismissed, setDismissed] = useState(false);

  if (!updateReady || dismissed) return null;

  return (
    <div
      role="status"
      className="fixed bottom-4 right-4 z-50 flex items-center gap-2 rounded-brutal border-2 border-black bg-brand px-3 py-2 shadow-brutal"
    >
      <RiRefreshLine className="text-base text-ink" />
      <span className="font-mono text-xs font-bold uppercase tracking-wider text-ink">
        New version
      </span>
      <button
        type="button"
        onClick={() => {
          serviceWorkerUpdates.apply();
        }}
        className="rounded-brutal border-2 border-black bg-surface px-2 py-0.5 font-mono text-xs font-bold uppercase tracking-wider text-ink shadow-brutal-xs transition-transform duration-100 active:translate-x-[1px] active:translate-y-[1px] active:shadow-none cursor-pointer"
      >
        Reload
      </button>
      <button
        type="button"
        aria-label="Dismiss"
        title="Dismiss"
        onClick={() => {
          setDismissed(true);
        }}
        className="text-ink/70 hover:text-ink cursor-pointer"
      >
        <RiCloseLine className="text-base" />
      </button>
    </div>
  );
}
