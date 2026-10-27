export const HTML_PREVIEW_LANGUAGE = "language-baybo-html";
export const HTML_PREVIEW_COLLAPSE_EVENT = "baybo:collapse-html-preview";
/// Native's left-edge swipe while a preview is full screen. The interactive pop
/// is held off there (PopGesture.swift) and the drag is streamed here instead,
/// so the swipe leaves the PREVIEW rather than the conversation.
export const HTML_PREVIEW_DRAG_BEGIN_EVENT = "baybo:html-preview-drag-begin";
export const HTML_PREVIEW_DRAG_MOVE_EVENT = "baybo:html-preview-drag-move";
export const HTML_PREVIEW_DRAG_END_EVENT = "baybo:html-preview-drag-end";
/// Set on <html> while a preview owns the screen — locks the thread's scroll
/// and lifts the `.md` clip that would otherwise cut a fixed child.
export const HTML_PREVIEW_MAXIMIZED_CLASS = "html-preview-maximized";
/// Where the thread was when a preview took the screen, kept as GEOMETRY: the
/// card's slot and how far below the viewport's top edge it sat. `slot` is null
/// while none is up. Locking the scroller can clamp its offset, so the live
/// offset is not where the reader is — and neither is the offset it had: a
/// foldable closing or opening under the preview rewraps every row of the
/// thread, and the same pixel then points at different content. The slot IS
/// what the reader left. The preview puts it back at its old top before it
/// morphs home, and a reading position captured meanwhile is measured as if it
/// already had.
export const previewParking: { slot: Element | null; top: number } = { slot: null, top: 0 };

export function parkThread(slot: Element): void {
  previewParking.slot = slot;
  previewParking.top = slot.getBoundingClientRect().top;
}

export function unparkThread(): void {
  previewParking.slot = null;
  previewParking.top = 0;
}

/// How far the document has to scroll (added to `scrollTop`) to put the parked
/// slot back at the top it was parked at, clamped to what the scroller can
/// reach. `null` while nothing is parked.
export function parkedThreadShift(): number | null {
  const slot = previewParking.slot;
  const el = document.scrollingElement;
  // `!el`, not `=== null`: an engine without `scrollingElement` (jsdom) has it
  // undefined.
  if (slot === null || !el) return null;
  const target = el.scrollTop + slot.getBoundingClientRect().top - previewParking.top;
  const reach = Math.max(0, el.scrollHeight - el.clientHeight);
  return Math.min(Math.max(0, target), reach) - el.scrollTop;
}
export const HTML_PREVIEW_URL_PREFIX =
  "baybo-transcript://localhost/html-preview/";

const BLOB_ID_PATTERN = /^sha256:[0-9a-f]{64}\.[0-9a-f]+$/;

export function htmlPreviewBlobId(
  className: string | undefined,
  source: string,
): string | null {
  if (!(className ?? "").split(/\s+/).includes(HTML_PREVIEW_LANGUAGE)) {
    return null;
  }
  const blobId = source.trim();
  return BLOB_ID_PATTERN.test(blobId) ? blobId : "";
}

export function htmlPreviewUrl(blobId: string, reload: number): string {
  return `${HTML_PREVIEW_URL_PREFIX}${blobId}?reload=${reload}`;
}
