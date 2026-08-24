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

/// Upload bytes and get back the id that names them.
///
/// A raw `fetch` rather than a generated client call: `/v1/blobs` is on the
/// channel router, co-hosted on the admin listener, and is not in
/// `docs/openapi.json` at all — so there is no typed method to call and
/// regenerating the spec will not produce one.
///
/// `owner` stamps the blob's `uploader_identity`. It is written once, at
/// upload, and can never be filled in later, which is the whole reason to
/// pass it now: nothing reclaims board blobs today, and a blob stored
/// without an owner could not be reclaimed even once something does.
export async function uploadBlob(
  baseUrl: string,
  token: string | null,
  file: File,
  owner?: { projectId: string },
): Promise<{ blobId: string; mime: string }> {
  const base = (baseUrl || '').replace(/\/+$/, '');
  const mime = file.type || 'application/octet-stream';
  const res = await fetch(`${base}/v1/blobs`, {
    method: 'POST',
    headers: {
      Authorization: `Bearer ${token ?? ''}`,
      'content-type': mime,
      ...(owner === undefined ? {} : { 'x-baybo-project': owner.projectId }),
    },
    body: file,
  });
  if (!res.ok) throw new Error(`upload failed: ${res.status}`);
  const data = (await res.json()) as { blob_id: string };
  return { blobId: data.blob_id, mime };
}

/// Save a blob to disk under a real name.
///
/// The counterpart every non-image attachment needs and the dashboard has
/// never had: `GET /v1/blobs/{id}` is bearer-gated, so an `<a href>` to it
/// downloads a 401 page. The bytes come through the same authenticated
/// fetch the thumbnails use, then a synthetic anchor carries the filename.
export async function downloadBlob(
  baseUrl: string,
  blobId: string,
  token: string | null,
  filename: string,
): Promise<void> {
  // Its own fetch, deliberately **not** `blobObjectUrl`: that map is a cache
  // for pictures the page keeps re-drawing, and it neither evicts nor revokes.
  // This path is reached only for a file the page will never draw — an image
  // gets a thumbnail, not a download chip — so routing through it would pin a
  // 50 MB log in the tab for the rest of the session to serve one click.
  const base = (baseUrl || '').replace(/\/+$/, '');
  const res = await fetch(`${base}/v1/blobs/${encodeURIComponent(blobId)}`, {
    headers: { Authorization: `Bearer ${token ?? ''}` },
  });
  if (!res.ok) throw new Error(`blob ${String(res.status)}`);
  const url = URL.createObjectURL(await res.blob());
  const link = document.createElement('a');
  link.href = url;
  link.download = filename;
  document.body.appendChild(link);
  link.click();
  link.remove();
  // After the click has been dispatched, not before: revoking synchronously
  // cancels the download in some browsers. One macrotask is enough.
  setTimeout(() => {
    URL.revokeObjectURL(url);
  }, 0);
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
