import { useEffect, useState } from 'react';

/// Blobs an `<img>` cannot fetch for itself.
///
/// `GET /v1/blobs/{id}` is bearer-gated and an `<img src>` carries no
/// Authorization header, so the bytes are fetched here and handed over as an
/// object URL.
///
/// The URLs are cached for the life of the page and never revoked. One
/// avatar is drawn on every card its owner holds, on every comment it wrote
/// and again in the roster; revoking on unmount would refetch the same bytes
/// once per drawing, and would hand a still-mounted `<img>` a URL that had
/// just been invalidated. What is retained is one object URL per distinct
/// blob — bounded by the size of the team.
const loaded = new Map<string, Promise<string>>();

/// Fetch a blob once and keep its object URL. Rejects if the blob is missing
/// or the fetch fails; the failure is *not* remembered, so a transient 500
/// does not blank an avatar until reload.
export async function blobObjectUrl(
  baseUrl: string,
  blobId: string,
  token: string | null,
): Promise<string> {
  const base = (baseUrl || '').replace(/\/+$/, '');
  const key = `${base}|${blobId}`;
  const already = loaded.get(key);
  if (already !== undefined) return already;
  const pending = (async () => {
    const res = await fetch(`${base}/v1/blobs/${encodeURIComponent(blobId)}`, {
      headers: { Authorization: `Bearer ${token ?? ''}` },
    });
    if (!res.ok) throw new Error(`blob ${res.status}`);
    return URL.createObjectURL(await res.blob());
  })();
  loaded.set(key, pending);
  void pending.catch(() => loaded.delete(key));
  return pending;
}

/// One blob, for a component that draws exactly one. Null while loading, on
/// failure, and without a blob id — callers render their own fallback.
export function useBlobUrl(
  blobId: string | null | undefined,
  baseUrl: string,
  token: string | null,
): string | null {
  const [url, setUrl] = useState<string | null>(null);
  useEffect(() => {
    setUrl(null);
    if (blobId == null || blobId === '') return;
    let alive = true;
    void blobObjectUrl(baseUrl, blobId, token).then(
      (resolved) => {
        if (alive) setUrl(resolved);
      },
      () => {
        // Callers render their fallback.
      },
    );
    return () => {
      alive = false;
    };
  }, [blobId, baseUrl, token]);
  return url;
}
