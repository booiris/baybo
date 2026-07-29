/**
 * Scroll a `[data-…]` anchor into view, retrying across a handful of frames.
 * A click often has to expand the target first (a collapsed turn/step mounts on
 * a later render, after the selection effect runs), so a single immediate
 * lookup races that re-render. Retrying a few frames covers it; it gives up
 * silently if the element never appears (e.g. the node is filtered out).
 *
 * The scroll is animated here rather than via `scrollIntoView({behavior:
 * 'smooth'})`: the native easing has no duration cap, so a jump across a long
 * trace crawls for over a second. This keeps the duration short and bounded no
 * matter the distance, and honours `prefers-reduced-motion` by jumping.
 */

const MIN_MS = 90;
const MAX_MS = 220;
// Higher = slower. Tuned so a short hop is near-instant and a full-trace jump
// still lands inside MAX_MS.
const MS_PER_PX = 0.12;

function scrollableAncestor(el: Element): HTMLElement | null {
  let node = el.parentElement;
  while (node) {
    const style = getComputedStyle(node);
    const scrolls = /auto|scroll|overlay/.test(style.overflowY);
    if (scrolls && node.scrollHeight > node.clientHeight) return node;
    node = node.parentElement;
  }
  return null;
}

function easeOutCubic(t: number): number {
  return 1 - Math.pow(1 - t, 3);
}

function animateScroll(container: HTMLElement, to: number): void {
  const from = container.scrollTop;
  const delta = to - from;
  if (Math.abs(delta) < 2) return;

  const reduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  if (reduced) {
    container.scrollTop = to;
    return;
  }

  const duration = Math.min(MAX_MS, Math.max(MIN_MS, Math.abs(delta) * MS_PER_PX));
  const start = performance.now();
  const step = (now: number) => {
    const t = Math.min(1, (now - start) / duration);
    container.scrollTop = from + delta * easeOutCubic(t);
    if (t < 1) requestAnimationFrame(step);
  };
  requestAnimationFrame(step);
}

export function scrollToAnchor(selector: string, block: 'start' | 'center' = 'start'): void {
  let tries = 0;
  const tick = () => {
    const el = document.querySelector(selector);
    if (el) {
      const container = scrollableAncestor(el);
      if (!container) {
        el.scrollIntoView({ block, behavior: 'auto' });
        return;
      }
      const elRect = el.getBoundingClientRect();
      const boxRect = container.getBoundingClientRect();
      // Offset of the element from the container's scroll origin.
      const offset = elRect.top - boxRect.top + container.scrollTop;
      const target =
        block === 'center' ? offset - container.clientHeight / 2 + elRect.height / 2 : offset;
      const max = container.scrollHeight - container.clientHeight;
      animateScroll(container, Math.max(0, Math.min(max, target)));
      return;
    }
    if (tries++ < 20) requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);
}
