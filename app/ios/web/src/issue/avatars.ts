import { blobObjectUrl } from "../bridge";

/// An agent's picture, fetched once per page and kept.
///
/// The same `requestBlob` round trip the attachment cards use — this page's
/// scheme handler is `staticOnly`, so a `blob://` URL is not an option here and
/// the bytes have to come over the bridge. What is different is the CARDINALITY:
/// one avatar appears on every row its author wrote, so a per-`<img>` fetch
/// would ask native for the same bytes a dozen times on one card. Hence a
/// promise cache keyed by blob id — the second caller joins the first request
/// rather than starting another.
///
/// Nothing is revoked. The object URLs live as long as the document, which is
/// one card: `IssueHost` builds a webview per card and tears it down on exit,
/// so the browser reclaims them with the page. A revoke-on-unmount would be
/// worse than useless here — the next row to mount wants the same URL.
const pending = new Map<string, Promise<string>>();

/// A blob id → an object URL for it. Rejects like `blobObjectUrl` does; a
/// failed fetch is remembered as failed, so a missing avatar costs one round
/// trip rather than one per row that draws it.
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
