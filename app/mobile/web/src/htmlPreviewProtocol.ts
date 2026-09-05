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
/// Root-relative on purpose: the preview iframe resolves it against the
/// document's own origin, which is the one its native host answers on —
/// `baybo-transcript://localhost` under the iOS scheme handler,
/// `https://appassets.androidplatform.net` under the Android asset
/// interceptor. Spelling either one here would pin the bundle to one shell.
export const HTML_PREVIEW_URL_PREFIX = "/html-preview/";

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
