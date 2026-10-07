import { RiGlobalLine } from 'react-icons/ri';
import type { TargetInfo } from '../../api/browserViewTypes';

/** Splits a URL into the origin (what the reader must be able to trust) and
 *  the rest. A URL that does not parse is shown whole as `rest`. */
export function splitUrl(url: string): { origin: string; rest: string } {
  try {
    const u = new URL(url);
    if (u.origin === 'null') return { origin: '', rest: url };
    // Rebuilt from the parsed parts, never sliced off the raw string: the raw
    // URL need not start with its origin (`https://bank.com@evil.com/`,
    // explicit default ports).
    return { origin: u.origin, rest: u.pathname + u.search + u.hash };
  } catch {
    return { origin: '', rest: url };
  }
}

// The address comes from CDP, never from the page's pixels, and sits in our
// own chrome above the canvas so a page cannot paint a fake one into view.
export function UrlBar({ target }: { target: TargetInfo | null }) {
  const { origin, rest } = splitUrl(target?.url ?? '');
  return (
    <div className="shrink-0 flex items-center gap-2 border-2 border-black rounded-brutal bg-surface px-2 py-1 font-mono text-xs min-w-0">
      <RiGlobalLine className="shrink-0 text-ink-soft" />
      {target ? (
        <>
          <span className="truncate min-w-0" title={target.url}>
            <span className="font-bold text-ink">{origin}</span>
            <span className="text-ink-soft">{rest}</span>
          </span>
          {target.title ? (
            <span className="ml-auto shrink-0 max-w-[40%] truncate text-ink-soft" title={target.title}>
              {target.title}
            </span>
          ) : null}
        </>
      ) : (
        <span className="text-ink-soft">No page</span>
      )}
    </div>
  );
}
