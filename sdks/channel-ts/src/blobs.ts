/**
 * Helpers for the gateway's `/v1/blobs/*` HTTP side-channel.
 *
 * Sidecars use these to push non-text media into the blob store
 * (`uploadBlob`) and to pull back blobs the agent has produced
 * (`fetchBlob`). Auth reuses the same `AURA_CHANNEL_TOKEN` the
 * WebSocket runner consumes — same loopback listener, same bearer.
 *
 * The base URL is derived from the WS URL the runner already knows:
 * `ws://127.0.0.1:<port>/v1/channel-ws` → `http://127.0.0.1:<port>`.
 * Pass an explicit `baseUrl` to override, or set `AURA_CHANNEL_URL`
 * (the gateway sets this automatically when it spawns a sidecar).
 */

const BLOB_PREFIX = "/v1/blobs";

export interface BlobClientOptions {
  /** Override the gateway base URL. Defaults to `AURA_CHANNEL_URL` derived. */
  baseUrl?: string;
  /** Override the bearer token. Defaults to `AURA_CHANNEL_TOKEN`. */
  token?: string;
  /** Optional `AbortSignal` to cancel the request. */
  signal?: AbortSignal;
  /**
   * Per-tenant credential id the platform event came in on. Required
   * for sidecar-flavor uploads — the gateway runs the same pairing
   * gate on `(channel_type, botId, userId)` that it runs on inbound
   * `Frame::Message` so an unpaired peer can't cause durable 100 MiB
   * blob writes. The SDK forwards it as the `x-aura-bot-id` header.
   */
  botId?: string;
  /**
   * Originating platform user id. Same role as `botId` — needed by
   * the pairing gate. Forwarded as `x-aura-user-id`.
   */
  userId?: string;
}

/**
 * Thrown by [`uploadBlob`] when the gateway refuses an upload because
 * the sender's `(channel_type, botId, userId)` triple has not been
 * paired yet. `code` is the same short string the user enters into
 * `aura pair approve <code>` to authorize the binding; sidecars
 * should surface it back to the platform user.
 */
export class BlobPairingRequiredError extends Error {
  readonly code: string;
  constructor(code: string) {
    super(`pairing required (code: ${code})`);
    this.name = "BlobPairingRequiredError";
    this.code = code;
  }
}

export interface UploadBlobResult {
  blobId: string;
}

export interface FetchBlobResult {
  bytes: Uint8Array;
  mimeType: string;
}

/** Upload `bytes` and return the content-addressed `blob_id`. */
export async function uploadBlob(
  bytes: Uint8Array,
  mimeType: string,
  opts: BlobClientOptions = {},
): Promise<UploadBlobResult> {
  const { baseUrl, token } = resolveAuth(opts);
  const url = `${baseUrl}${BLOB_PREFIX}`;
  const headers: Record<string, string> = {
    "content-type": mimeType,
    "x-aura-channel-token": token,
  };
  if (opts.botId) headers["x-aura-bot-id"] = opts.botId;
  if (opts.userId) headers["x-aura-user-id"] = opts.userId;
  // Use a Blob to satisfy fetch's strict body type without forcing a copy.
  const body = new Blob([bytes], { type: mimeType });
  const res = await fetchWithSignal(url, {
    method: "POST",
    headers,
    body,
    ...(opts.signal ? { signal: opts.signal } : {}),
  });
  if (res.status === 403) {
    // Either a pairing prompt or some other forbidden — try parsing
    // the JSON body so a typed error reaches the sidecar.
    let parsed: { error?: unknown; code?: unknown } | null = null;
    try {
      parsed = (await res.json()) as { error?: unknown; code?: unknown };
    } catch {
      parsed = null;
    }
    if (
      parsed
      && parsed.error === "pairing_required"
      && typeof parsed.code === "string"
    ) {
      throw new BlobPairingRequiredError(parsed.code);
    }
    throw new Error(`uploadBlob: 403 ${res.statusText} (${url})`);
  }
  if (!res.ok) {
    throw new Error(
      `uploadBlob: ${res.status} ${res.statusText} (${url})`,
    );
  }
  const json = (await res.json()) as { blob_id?: unknown };
  if (typeof json.blob_id !== "string") {
    throw new Error("uploadBlob: response missing blob_id");
  }
  return { blobId: json.blob_id };
}

/** Fetch a blob's bytes by id. Throws on 404 / any non-2xx. */
export async function fetchBlob(
  blobId: string,
  opts: BlobClientOptions = {},
): Promise<FetchBlobResult> {
  const { baseUrl, token } = resolveAuth(opts);
  const url = `${baseUrl}${BLOB_PREFIX}/${encodeURIComponent(blobId)}`;
  const res = await fetchWithSignal(url, {
    method: "GET",
    headers: { "x-aura-channel-token": token },
    ...(opts.signal ? { signal: opts.signal } : {}),
  });
  if (!res.ok) {
    throw new Error(`fetchBlob: ${res.status} ${res.statusText} (${url})`);
  }
  const buf = new Uint8Array(await res.arrayBuffer());
  const mimeType = res.headers.get("content-type") ?? "application/octet-stream";
  return { bytes: buf, mimeType };
}

function resolveAuth(opts: BlobClientOptions): { baseUrl: string; token: string } {
  const token = opts.token ?? process.env["AURA_CHANNEL_TOKEN"];
  if (!token) {
    throw new Error(
      "blobs: missing token — pass opts.token or set AURA_CHANNEL_TOKEN",
    );
  }
  const baseUrl = opts.baseUrl ?? deriveBaseUrl();
  return { baseUrl, token };
}

/**
 * Convert the WS URL the runner uses into the matching HTTP origin.
 * `AURA_CHANNEL_URL` is set by the gateway's `ChannelSpawner` to
 * `ws://127.0.0.1:<port>/v1/channel-ws`, so this is a literal scheme
 * swap with the path stripped.
 */
function deriveBaseUrl(): string {
  const wsUrl = process.env["AURA_CHANNEL_URL"];
  if (!wsUrl) {
    throw new Error(
      "blobs: missing baseUrl — pass opts.baseUrl or set AURA_CHANNEL_URL",
    );
  }
  let parsed: URL;
  try {
    parsed = new URL(wsUrl);
  } catch (e) {
    throw new Error(`blobs: invalid AURA_CHANNEL_URL ${wsUrl}: ${String(e)}`);
  }
  const httpProto =
    parsed.protocol === "wss:" ? "https:" : parsed.protocol === "ws:" ? "http:" : parsed.protocol;
  return `${httpProto}//${parsed.host}`;
}

interface FetchInit {
  method: string;
  headers: Record<string, string>;
  body?: Blob;
  signal?: AbortSignal;
}

async function fetchWithSignal(url: string, init: FetchInit): Promise<Response> {
  // Node 22+ ships fetch as a global; the SDK's package.json pins
  // `engines.node >= 22` so we don't have to fall back to undici.
  const res = await fetch(url, {
    method: init.method,
    headers: init.headers,
    ...(init.body !== undefined ? { body: init.body } : {}),
    ...(init.signal ? { signal: init.signal } : {}),
  });
  return res;
}
