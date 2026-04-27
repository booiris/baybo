/**
 * CDN URL builders for Weixin upload / download.
 *
 * The iLink server returns a `full_url` field in newer protocol
 * versions; we prefer that and only fall back to client-side
 * construction when the field is missing. The fallback exists because
 * older deployments don't populate `full_url` consistently —
 * disabling it (`ENABLE_CDN_URL_FALLBACK = false`) is the right call
 * once every server we talk to ships the field.
 */

/** Set false to refuse client-side URL construction when the server omits `full_url`. */
export const ENABLE_CDN_URL_FALLBACK = true;

/** Build a CDN download URL from `encrypt_query_param`. */
export function buildCdnDownloadUrl(encryptedQueryParam: string, cdnBaseUrl: string): string {
  return `${cdnBaseUrl}/download?encrypted_query_param=${encodeURIComponent(encryptedQueryParam)}`;
}

/** Build a CDN upload URL from `upload_param` + filekey. */
export function buildCdnUploadUrl(params: {
  cdnBaseUrl: string;
  uploadParam: string;
  filekey: string;
}): string {
  return `${params.cdnBaseUrl}/upload?encrypted_query_param=${encodeURIComponent(params.uploadParam)}&filekey=${encodeURIComponent(params.filekey)}`;
}
