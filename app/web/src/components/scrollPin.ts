import { useCallback, useEffect, useRef, type RefObject } from 'react';

/// How far off the bottom still counts as being at it. A reader a line up from
/// the newest entry is still watching it arrive, and a pane that only re-anchors
/// on an exact match spends most of its life un-pinned over a sub-pixel
/// remainder.
export const BOTTOM_SLACK_PX = 64;

/// Whether a scroller is parked at its newest edge — the one question every
/// stick-to-bottom surface asks, and the one it must not answer twice. What
/// hangs off it is whether an arrival scrolls the reader or merely raises a
/// pill at them, so two spellings drifting apart is a pane that yanks in one
/// place and goes quiet in another.
export function atBottom(el: HTMLElement, slackPx: number = BOTTOM_SLACK_PX): boolean {
  return el.scrollHeight - el.scrollTop - el.clientHeight <= slackPx;
}

/// Holds a scroller's newest edge through height that lands AFTER the commit
/// that appended it: an attachment thumbnail swapping its placeholder for the
/// real image (the blob is fetched by the tag's own component, so the box has
/// no reserved size), the webfont swap, KaTeX. On a cold open those all resolve
/// after the first paint, which left the reader parked above the newest entry —
/// and a warm reload hid it, since the bytes are already cached and land before
/// the pin.
///
/// Returns a ref callback for the scroller's **content** box, not the scroller:
/// the scroller's own border box never changes size, so observing it reports
/// nothing. Attached by callback rather than by ref object because the observed
/// element is usually inside a branch that unmounts (an empty state, a loading
/// state), and a plain ref would leave the observer pointed at a dead node.
export function useHoldBottomEdge(
  scroller: RefObject<HTMLElement | null>,
  /// Live pin state. Growth under a reader in scroll-back must not yank them
  /// down — the browser already holds their position for it.
  pinned: RefObject<boolean>,
): (node: Element | null) => void {
  const observer = useRef<ResizeObserver | null>(null);
  useEffect(() => () => observer.current?.disconnect(), []);
  return useCallback(
    (node: Element | null) => {
      observer.current?.disconnect();
      observer.current = null;
      if (node === null || typeof ResizeObserver === 'undefined') return;
      const next = new ResizeObserver(() => {
        const box = scroller.current;
        if (box === null || !pinned.current) return;
        box.scrollTop = box.scrollHeight;
      });
      next.observe(node);
      observer.current = next;
    },
    [scroller, pinned],
  );
}
