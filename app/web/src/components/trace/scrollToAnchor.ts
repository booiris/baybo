/**
 * Scroll a `[data-…]` anchor into view, retrying across a handful of frames.
 * A click often has to expand the target first (a collapsed job/step mounts on
 * a later render, after the selection effect runs), so a single immediate
 * lookup races that re-render. Retrying a few frames covers it; it gives up
 * silently if the element never appears (e.g. the node is filtered out).
 */
export function scrollToAnchor(selector: string, block: ScrollLogicalPosition = 'start'): void {
  let tries = 0;
  const tick = () => {
    const el = document.querySelector(selector);
    if (el) {
      el.scrollIntoView({ block, behavior: 'smooth' });
      return;
    }
    if (tries++ < 20) requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);
}
