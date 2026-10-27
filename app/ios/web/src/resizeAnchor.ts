/// Holding a reader's place while the page under them reflows — the one
/// anchoring scheme the transcript and the issue page share, so the two cannot
/// drift. WebKit has no scroll anchoring: when rows above the reader change
/// height, `scrollTop` stays the same pixel value and the content under it
/// moves. The only fix is to remember a ROW and where it sat, and write
/// `scrollTop` so it sits there again.

/// One element to hold a place by: its id (the value of the attribute it was
/// found under) and how far its top sat below the scroller's top edge. `part`
/// refines a row taller than the screen: which of its blocks crossed the edge
/// (its index among the row's matches of the refining selector) and where that
/// block's top sat. A width change rewraps every line of a long row, so the
/// row's own offset lands paragraphs away from where the reader was; the block
/// that held the edge lands on it.
export type ScrollAnchor = {
  id: string;
  offset: number;
  part?: { index: number; offset: number };
};

/// The first element carrying `attr` that is still on screen — its bottom below
/// `viewportTop` — and where its top sits relative to that edge. Elements are
/// taken in document order, so a page has to put the attribute on rows that do
/// not contain one another. With `parts`, a row that straddles the edge is
/// refined to the first of its `parts` blocks still on screen, and one with
/// none of them left on screen gives way to the row after it. `null` when none
/// is on screen.
export function captureAnchor(
  root: ParentNode,
  attr: string,
  viewportTop = 0,
  parts?: string,
): ScrollAnchor | null {
  for (const el of root.querySelectorAll(`[${attr}]`)) {
    const rect = el.getBoundingClientRect();
    if (rect.bottom <= viewportTop) continue;
    const id = el.getAttribute(attr);
    if (id === null) return null;
    const anchor: ScrollAnchor = { id, offset: rect.top - viewportTop };
    if (parts !== undefined && rect.top < viewportTop) {
      const blocks = el.querySelectorAll(parts);
      let laidOut = 0;
      for (let index = 0; index < blocks.length; index++) {
        const block = blocks[index].getBoundingClientRect();
        // A block with no box (hidden, not laid out) sits nowhere.
        if (block.height === 0) continue;
        laidOut += 1;
        if (block.bottom <= viewportTop) continue;
        anchor.part = { index, offset: block.top - viewportTop };
        break;
      }
      // Only the row's tail (its timestamp, its meta) is left on screen: the
      // next row's top is, and holds the place exactly — the row's own top is
      // a whole rewrapped reply away.
      if (anchor.part === undefined && laidOut > 0) continue;
    }
    return anchor;
  }
  return null;
}

/// How far to move `scrollTop` (added to it) to put `anchor` back where it was
/// captured — by its block when it has one still there (same `parts` as the
/// capture), else by the row. `null` when the row is gone — there is nothing
/// left to hold a position against, and each caller knows its own fallback.
export function anchorShift(
  root: ParentNode,
  attr: string,
  anchor: ScrollAnchor,
  viewportTop = 0,
  parts?: string,
): number | null {
  const node = root.querySelector(`[${attr}="${CSS.escape(anchor.id)}"]`);
  if (node === null) return null;
  if (anchor.part !== undefined && parts !== undefined) {
    const blocks = node.querySelectorAll(parts);
    if (anchor.part.index < blocks.length) {
      const block = blocks[anchor.part.index].getBoundingClientRect();
      return block.top - viewportTop - anchor.part.offset;
    }
  }
  return node.getBoundingClientRect().top - viewportTop - anchor.offset;
}

export type WidthResize<T> = {
  /// The content box's width changed, so its rows have wrapped anew.
  widthChanged: boolean;
  /// The place captured at the PREVIOUS layout — before this reflow.
  before: T;
};

/// Watch `box` for size changes, handing `onResize` the reader's place as it
/// was BEFORE the change.
///
/// A ResizeObserver runs after layout, so a place read inside its callback
/// already describes the new width — and so does one read anywhere in the
/// frame the viewport changed in, since reading geometry forces that layout.
/// The only pre-reflow place is one taken earlier. So it is kept warm: on every
/// scroll of `scroller` (once per frame) and after every observed resize (the
/// box changes size whenever its content does). A scroll capture that finds the
/// box's width changed under it skips that one frame — a scroll event can land
/// in the very frame the viewport changed (WebKit clamping `scrollTop`), and the
/// reflow it would read belongs to the resize the observer is about to report;
/// the place it would overwrite is the one that resize needs. It skips rather
/// than waiting for the observer, which never fires at all when the viewport
/// changes and the capped content width does not.
///
/// Width is the observed CONTENT width: inside a capped reading band the
/// viewport can change while the rows' own width does not, and then nothing
/// wrapped. Returns the disconnect.
export function holdPlaceAcrossWidth<T>(
  box: Element,
  scroller: EventTarget,
  capture: () => T,
  onResize: (resize: WidthResize<T>) => void,
): () => void {
  let contentWidth: number | null = null;
  let boxWidth = box.getBoundingClientRect().width;
  let before = capture();
  let frame: number | null = null;

  const ro = new ResizeObserver((entries) => {
    const entry = entries[entries.length - 1];
    const next = entry.contentRect.width;
    const widthChanged = contentWidth !== null && next !== contentWidth;
    contentWidth = next;
    boxWidth = box.getBoundingClientRect().width;
    onResize({ widthChanged, before });
    before = capture();
  });
  ro.observe(box);

  const onScroll = () => {
    if (frame !== null) return;
    frame = requestAnimationFrame(() => {
      frame = null;
      const width = box.getBoundingClientRect().width;
      if (width !== boxWidth) {
        boxWidth = width;
        return;
      }
      before = capture();
    });
  };
  scroller.addEventListener("scroll", onScroll, { passive: true });

  return () => {
    ro.disconnect();
    scroller.removeEventListener("scroll", onScroll);
    if (frame !== null) cancelAnimationFrame(frame);
  };
}
