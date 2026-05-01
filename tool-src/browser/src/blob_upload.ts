// Optional streaming upload to the gateway's `/v1/blobs` endpoint.
//
// Large screenshots (>2 MiB) bypass the 16 MiB-cap inline `Image`
// content path and POST their bytes to the gateway directly; the
// returned `blob_id` is then surfaced to Aura via an MCP
// `ResourceLink` whose URI is `aura://blob/<blob_id>`. The Rust
// content adapter parses that URI and constructs a `ContentBlock::Image`
// from the existing blob — no second download.
//
// The upload destination is resolved lazily on first call:
//
//   - `AURA_CHANNEL_PORT_FILE` points at `<workspace>/state/channel.port`,
//     a tiny text file the gateway's channel TCP listener writes after
//     it binds. Reading it lazily sidesteps the boot-order race —
//     the file exists by the time any tool call lands.
//   - `AURA_BLOB_UPLOAD_TOKEN` is the channel-token registered against
//     a `tool/<sidecar>` label. Aura's `AuthedClient::Tool` bypasses
//     the per-bot pairing gate on `/v1/blobs`.
//
// Either env missing → uploads disabled (every screenshot inlines).

import { readFile } from "node:fs/promises";

const INLINE_THRESHOLD_BYTES = 2 * 1024 * 1024;
const PORT_FILE_CACHE_TTL_MS = 5_000;

interface CachedPort {
  port: number;
  fetchedAt: number;
}

let cachedPort: CachedPort | null = null;

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

async function resolvePort(portFile: string): Promise<number> {
  // The port file rarely changes during a process lifetime (only on
  // gateway restart), so cache for a few seconds to avoid a syscall
  // per upload. On a stale read we'll get a 401/connection refused
  // and the caller throws — operator can re-screenshot and the cache
  // refreshes.
  const now = Date.now();
  if (cachedPort && now - cachedPort.fetchedAt < PORT_FILE_CACHE_TTL_MS) {
    return cachedPort.port;
  }
  const raw = await readFile(portFile, "utf8");
  const port = Number.parseInt(raw.trim(), 10);
  if (!Number.isFinite(port) || port <= 0 || port > 65_535) {
    throw new Error(`AURA_CHANNEL_PORT_FILE='${portFile}' is not a valid port`);
  }
  cachedPort = { port, fetchedAt: now };
  return port;
}

/**
 * Upload `bytes` to the gateway's `/v1/blobs` endpoint and return the
 * resulting `blob_id`. Returns `null` when:
 *
 *   - the upload env is not set (gateway boot didn't wire it, or this
 *     is a non-gateway run like the TS unit test bundle), or
 *   - `bytes.length` is at or below the inline threshold (caller
 *     should fall back to inlining the bytes as MCP `Image` content).
 *
 * Throws on transport failure or non-2xx response — the caller surfaces
 * the failure as the tool-call error so the agent sees what happened
 * rather than silently dropping the image.
 */
export async function maybeUpload(
  bytes: Buffer,
  mimeType: string,
): Promise<string | null> {
  if (bytes.length <= INLINE_THRESHOLD_BYTES) return null;
  const cfg = readUploadConfig();
  if (!cfg) return null;

  const port = await resolvePort(cfg.portFile);
  const url = `http://127.0.0.1:${port}/v1/blobs`;

  const res = await fetch(url, {
    method: "POST",
    headers: {
      "x-aura-channel-token": cfg.token,
      "content-type": mimeType,
    },
    // Node fetch accepts a Buffer directly; the gateway's
    // `put_stream` reads it as a single chunk into the content-
    // addressed blob store.
    body: new Uint8Array(bytes),
  });

  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(
      `blob upload failed: ${res.status} ${res.statusText}${body ? `: ${body}` : ""}`,
    );
  }
  const json = (await res.json()) as { blob_id?: unknown };
  if (typeof json.blob_id !== "string" || json.blob_id.length === 0) {
    throw new Error("blob upload response missing blob_id");
  }
  return json.blob_id;
}

/** The threshold bytes above which `maybeUpload` will attempt a POST. */
export const INLINE_THRESHOLD = INLINE_THRESHOLD_BYTES;
