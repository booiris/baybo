import { useEffect, useState } from 'react';
import { RiImageLine, RiLoader4Line } from 'react-icons/ri';

// Renders a blob-backed image attachment. `<img>` can't carry the
// Authorization header, so we fetch the blob ourselves and hand the bitmap to
// the tag as an object URL.
export function AttachmentImage({
  blobId,
  alt,
  baseUrl,
  adminToken,
}: {
  blobId: string;
  alt: string;
  baseUrl: string;
  adminToken: string | null;
}) {
  const [url, setUrl] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    let cancelled = false;
    let objectUrl: string | null = null;
    void (async () => {
      try {
        const base = (baseUrl || '').replace(/\/+$/, '');
        const res = await fetch(`${base}/v1/blobs/${encodeURIComponent(blobId)}`, {
          headers: { Authorization: `Bearer ${adminToken ?? ''}` },
        });
        if (!res.ok) throw new Error(`blob ${res.status}`);
        const blob = await res.blob();
        if (cancelled) return;
        objectUrl = URL.createObjectURL(blob);
        setUrl(objectUrl);
      } catch {
        if (!cancelled) setFailed(true);
      }
    })();
    return () => {
      cancelled = true;
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [blobId, baseUrl, adminToken]);

  if (failed) {
    return (
      <span className="flex items-center gap-1.5 px-2 py-1 bg-canvas border-2 border-black rounded-md font-mono text-[0.7rem] max-w-full">
        <RiImageLine className="text-sm shrink-0" />
        <span className="truncate">{alt}</span>
      </span>
    );
  }
  if (!url) {
    return (
      <div className="flex items-center justify-center h-24 w-24 bg-canvas border-2 border-black rounded-md">
        <RiLoader4Line className="text-lg animate-spin text-ink-soft" />
      </div>
    );
  }
  return (
    <img
      src={url}
      alt={alt}
      className="max-h-48 max-w-full rounded-md border-2 border-black object-contain"
    />
  );
}
