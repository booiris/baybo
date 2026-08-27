import { blobObjectUrl } from "../bridge";

// Promise-cache by blob id so every row for one author joins the same native
// fetch. URLs live for the pooled document and are reclaimed with it.
const pending = new Map<string, Promise<string>>();

export function avatarUrl(blobId: string): Promise<string> {
  const known = pending.get(blobId);
  if (known !== undefined) return known;
  // The mime is a fallback for a `blobResult` that carries none; every avatar
  // the gateway stores is an image and the real type rides the response.
  const fetching = blobObjectUrl(blobId, "image/png");
  pending.set(blobId, fetching);
  return fetching;
}

/// Drop the cache. Tests only — the app's cache dies with the document.
export function forgetAvatars(): void {
  pending.clear();
}
