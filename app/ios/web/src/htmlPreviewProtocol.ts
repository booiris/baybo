export const HTML_PREVIEW_LANGUAGE = "language-baybo-html";
export const HTML_PREVIEW_COLLAPSE_EVENT = "baybo:collapse-html-preview";
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
