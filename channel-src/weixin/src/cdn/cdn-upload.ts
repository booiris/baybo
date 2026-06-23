/**
 * Encrypt + POST one buffer to the iLink CDN. The caller has already
 * called `getUploadUrl` (so we have either `upload_full_url` or
 * `upload_param + filekey`); we encrypt with the shared AES-128-ECB
 * key and bounce up to `UPLOAD_MAX_RETRIES` times on transient
 * server errors.
 */
import type { Logger } from "@baybo/channel-sdk";

import { encryptAesEcb } from "./aes-ecb.js";
import { buildCdnUploadUrl } from "./cdn-url.js";

/** Cap on retry attempts for one CDN POST. */
const UPLOAD_MAX_RETRIES = 3;

/**
 * Upload `buf` to the CDN. Returns the `downloadParam` the iLink
 * server stamps in the `x-encrypted-param` header — that string goes
 * into `CDNMedia.encrypt_query_param` on the resulting message item.
 *
 * Client errors (4xx) abort immediately; server errors (5xx, network)
 * retry with no backoff (the iLink CDN typically recovers fast and a
 * deliberate spin avoids extending the user-visible reply latency).
 */
export async function uploadBufferToCdn(params: {
  buf: Buffer;
  /** From `getUploadUrl.upload_full_url`. Takes precedence over `uploadParam`. */
  uploadFullUrl?: string;
  uploadParam?: string;
  filekey: string;
  cdnBaseUrl: string;
  label: string;
  aeskey: Buffer;
  logger: Logger;
}): Promise<{ downloadParam: string }> {
  const { buf, uploadFullUrl, uploadParam, filekey, cdnBaseUrl, label, aeskey, logger } = params;
  const ciphertext = encryptAesEcb(buf, aeskey);
  const trimmedFull = uploadFullUrl?.trim();
  let cdnUrl: string;
  if (trimmedFull) {
    cdnUrl = trimmedFull;
  } else if (uploadParam) {
    cdnUrl = buildCdnUploadUrl({ cdnBaseUrl, uploadParam, filekey });
  } else {
    throw new Error(`${label}: CDN upload URL missing (need upload_full_url or upload_param)`);
  }
  logger.debug(`${label}: CDN POST ciphertextSize=${ciphertext.length}`);

  let downloadParam: string | undefined;
  let lastError: unknown;

  for (let attempt = 1; attempt <= UPLOAD_MAX_RETRIES; attempt++) {
    try {
      const res = await fetch(cdnUrl, {
        method: "POST",
        headers: { "Content-Type": "application/octet-stream" },
        body: new Uint8Array(ciphertext),
      });
      if (res.status >= 400 && res.status < 500) {
        const errMsg = res.headers.get("x-error-message") ?? (await res.text());
        logger.error(
          `${label}: CDN client error attempt=${attempt} status=${res.status} errMsg=${errMsg}`,
        );
        throw new Error(`CDN upload client error ${res.status}: ${errMsg}`);
      }
      if (res.status !== 200) {
        const errMsg = res.headers.get("x-error-message") ?? `status ${res.status}`;
        logger.error(
          `${label}: CDN server error attempt=${attempt} status=${res.status} errMsg=${errMsg}`,
        );
        throw new Error(`CDN upload server error: ${errMsg}`);
      }
      downloadParam = res.headers.get("x-encrypted-param") ?? undefined;
      if (!downloadParam) {
        logger.error(
          `${label}: CDN response missing x-encrypted-param header attempt=${attempt}`,
        );
        throw new Error("CDN upload response missing x-encrypted-param header");
      }
      logger.debug(`${label}: CDN upload success attempt=${attempt}`);
      break;
    } catch (err) {
      lastError = err;
      if (err instanceof Error && err.message.includes("client error")) throw err;
      if (attempt < UPLOAD_MAX_RETRIES) {
        logger.error(`${label}: attempt ${attempt} failed, retrying... err=${String(err)}`);
      } else {
        logger.error(`${label}: all ${UPLOAD_MAX_RETRIES} attempts failed err=${String(err)}`);
      }
    }
  }

  if (!downloadParam) {
    throw lastError instanceof Error
      ? lastError
      : new Error(`CDN upload failed after ${UPLOAD_MAX_RETRIES} attempts`);
  }
  return { downloadParam };
}
