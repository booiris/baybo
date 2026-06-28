/**
 * Download CDN media + AES-128-ECB decrypt. Two flavours: encrypted
 * (image / voice / file / video) and plain (older image attachments
 * that don't carry an `aes_key`). The decrypt path also has to deal
 * with two on-the-wire encodings of the AES key — see [`parseAesKey`].
 */
import type { Logger } from "@baybo/channel-sdk";

import { decryptAesEcb } from "./aes-ecb.js";
import { ENABLE_CDN_URL_FALLBACK, buildCdnDownloadUrl } from "./cdn-url.js";

/** Download raw bytes from the CDN with no decryption. */
async function fetchCdnBytes(url: string, label: string, logger: Logger): Promise<Buffer> {
  let res: Response;
  try {
    res = await fetch(url);
  } catch (err) {
    const cause =
      (err as NodeJS.ErrnoException).cause
      ?? (err as NodeJS.ErrnoException).code
      ?? "(no cause)";
    logger.error(
      `${label}: fetch network error url=${url} err=${String(err)} cause=${String(cause)}`,
    );
    throw err;
  }
  logger.debug(`${label}: response status=${res.status} ok=${res.ok}`);
  if (!res.ok) {
    const body = await res.text().catch(() => "(unreadable)");
    const msg = `${label}: CDN download ${res.status} ${res.statusText} body=${body}`;
    logger.error(msg);
    throw new Error(msg);
  }
  return Buffer.from(await res.arrayBuffer());
}

/**
 * Parse `CDNMedia.aes_key` into a raw 16-byte AES key. Two encodings
 * are observed on the wire:
 *   - `base64(raw 16 bytes)`            — image keys
 *   - `base64(hex string of 16 bytes)`  — file / voice / video keys
 *
 * The second case base64-decodes to 32 ASCII hex chars, which then
 * have to be parsed as hex to recover the actual 16-byte key. The
 * length-and-hex-shape check below picks the right path without
 * needing a flag from the caller.
 */
function parseAesKey(aesKeyBase64: string, label: string, logger: Logger): Buffer {
  const decoded = Buffer.from(aesKeyBase64, "base64");
  if (decoded.length === 16) {
    return decoded;
  }
  if (decoded.length === 32 && /^[0-9a-fA-F]{32}$/.test(decoded.toString("ascii"))) {
    return Buffer.from(decoded.toString("ascii"), "hex");
  }
  const msg = `${label}: aes_key must decode to 16 raw bytes or 32-char hex string, got ${decoded.length} bytes (base64="${aesKeyBase64}")`;
  logger.error(msg);
  throw new Error(msg);
}

/**
 * Download + AES-128-ECB decrypt a CDN media file. Returns plaintext.
 *
 * `aesKeyBase64` is the `CDNMedia.aes_key` JSON field — see
 * [`parseAesKey`] for the supported encodings. When `fullUrl` is set
 * we prefer it; otherwise we fall back to the client-side URL builder
 * iff [`ENABLE_CDN_URL_FALLBACK`] is on.
 */
export async function downloadAndDecryptBuffer(params: {
  encryptedQueryParam: string;
  aesKeyBase64: string;
  cdnBaseUrl: string;
  label: string;
  fullUrl?: string;
  logger: Logger;
}): Promise<Buffer> {
  const { encryptedQueryParam, aesKeyBase64, cdnBaseUrl, label, fullUrl, logger } = params;
  const key = parseAesKey(aesKeyBase64, label, logger);
  let url: string;
  if (fullUrl) {
    url = fullUrl;
  } else if (ENABLE_CDN_URL_FALLBACK) {
    url = buildCdnDownloadUrl(encryptedQueryParam, cdnBaseUrl);
  } else {
    throw new Error(`${label}: fullUrl is required (CDN URL fallback is disabled)`);
  }
  logger.debug(`${label}: fetching url=${url}`);
  const encrypted = await fetchCdnBytes(url, label, logger);
  logger.debug(`${label}: downloaded ${encrypted.byteLength} bytes, decrypting`);
  const decrypted = decryptAesEcb(encrypted, key);
  logger.debug(`${label}: decrypted ${decrypted.length} bytes`);
  return decrypted;
}

/** Download plain (unencrypted) bytes from the CDN. */
export async function downloadPlainCdnBuffer(params: {
  encryptedQueryParam: string;
  cdnBaseUrl: string;
  label: string;
  fullUrl?: string;
  logger: Logger;
}): Promise<Buffer> {
  const { encryptedQueryParam, cdnBaseUrl, label, fullUrl, logger } = params;
  let url: string;
  if (fullUrl) {
    url = fullUrl;
  } else if (ENABLE_CDN_URL_FALLBACK) {
    url = buildCdnDownloadUrl(encryptedQueryParam, cdnBaseUrl);
  } else {
    throw new Error(`${label}: fullUrl is required (CDN URL fallback is disabled)`);
  }
  logger.debug(`${label}: fetching url=${url}`);
  return fetchCdnBytes(url, label, logger);
}
