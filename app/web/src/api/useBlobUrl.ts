import { useEffect, useState } from 'react';

// `<img>` can't carry the Authorization header, so fetch the blob and hand
// the bitmap over as an object URL (same pattern as the chat
// AttachmentImage). Returns null while loading, on failure, or without a
// blob id — callers fall back to their default portrait.
export function useBlobUrl(
  blobId: string | null,
  baseUrl: string,
  token: string | null,
): string | null {
  const [url, setUrl] = useState<string | null>(null);
  useEffect(() => {
    setUrl(null);
    if (!blobId) return;
    let cancelled = false;
    let objectUrl: string | null = null;
    void (async () => {
      try {
        const base = (baseUrl || '').replace(/\/+$/, '');
        const res = await fetch(`${base}/v1/blobs/${encodeURIComponent(blobId)}`, {
          headers: { Authorization: `Bearer ${token ?? ''}` },
        });
        if (!res.ok) throw new Error(`blob ${res.status}`);
        const blob = await res.blob();
        if (cancelled) return;
        objectUrl = URL.createObjectURL(blob);
        setUrl(objectUrl);
      } catch {
        // Callers render their fallback.
      }
    })();
    return () => {
      cancelled = true;
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [blobId, baseUrl, token]);
  return url;
}
