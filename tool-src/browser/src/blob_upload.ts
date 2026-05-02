// Optional streaming upload to the gateway's `/v1/blobs` endpoint.
//
// Wraps `@aura/channel-sdk`'s `uploadBlob` so the wire protocol stays
// in one place — sidecars don't reimplement the POST shape, the
// `x-aura-channel-token` header convention, the typed pairing-error
// path, or the `{blob_id}` response parsing.
//
// We diverge from the channel-sidecar usage on URL discovery: channel
// sidecars receive `AURA_CHANNEL_URL` directly, but the embedded
// browser MCP child is spawned before `ChannelServer::bind` runs, so
// the gateway hands it a *port file path* instead. The first upload
// reads the file (lazy) to resolve `127.0.0.1:<port>` and feeds that
// to the SDK as `opts.baseUrl`.
//
// Either env missing → `maybeUpload` returns null and the caller
// inlines the bytes as MCP `Image` content.

import { readFile } from "node:fs/promises";

import { uploadBlob } from "@aura/channel-sdk";

const INLINE_THRESHOLD_BYTES = 2 * 1024 * 1024;

interface UploadConfig {
  portFile: string;
  token: string;
}

function readUploadConfig(): UploadConfig | null {
  const portFile = process.env["AURA_CHANNEL_PORT_FILE"];
  const token = process.env["AURA_BLOB_UPLOAD_TOKEN"];
  if (!portFile || !token) return null;
  return { portFile, token };
}

async function resolveBaseUrl(portFile: string): Promise<string> {
  // Read fresh on every call. The port file can't change during the
  // child's lifetime (a gateway restart kills the child too), so a
  // cache adds complexity without real benefit — `readFile` of a
  // <10-byte local file is microseconds.
  const raw = await readFile(portFile, "utf8");
  const port = Number.parseInt(raw.trim(), 10);
  if (!Number.isFinite(port) || port <= 0 || port > 65_535) {
    throw new Error(`AURA_CHANNEL_PORT_FILE='${portFile}' is not a valid port`);
  }
  return `http://127.0.0.1:${port}`;
}

/**
 * Upload `bytes` to the gateway's `/v1/blobs` endpoint and return the
 * resulting `blob_id`. Returns `null` when the upload env isn't set
 * (non-gateway runs, e.g. the TS unit-test bundle) OR when
 * `bytes.length` is at or below the inline threshold (caller should
 * fall back to inlining as MCP `Image` content).
 *
 * Throws on transport / non-2xx so the agent sees the failure rather
 * than silently dropping the image.
 */
export async function maybeUpload(
  bytes: Buffer,
  mimeType: string,
): Promise<string | null> {
  if (bytes.length <= INLINE_THRESHOLD_BYTES) return null;
  const cfg = readUploadConfig();
  if (!cfg) return null;

  const baseUrl = await resolveBaseUrl(cfg.portFile);
  const result = await uploadBlob(bytes, mimeType, {
    baseUrl,
    token: cfg.token,
  });
  return result.blobId;
}
