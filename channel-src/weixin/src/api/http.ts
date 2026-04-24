import crypto from "node:crypto";

// Inlined at bundle time via tsconfig's `resolveJsonModule` (bun's
// bundler replaces this with a const object literal in the single-file
// output). A runtime `readFileSync("../../package.json")` would break
// once the sidecar is extracted to
// `$XDG_CACHE_HOME/aura/sidecars/weixin-<hash>.js` — a single JS file
// with no sibling `package.json` — silently zeroing the iLink app
// identity on every outbound request. The `with { type: "json" }`
// attribute is required by Node 22+ at runtime (used by the
// `node --test` suite); tsc needs `module: ESNext` for the syntax.
import pkg from "../../package.json" with { type: "json" };

import type { BaseInfo } from "./types.js";

const CHANNEL_VERSION: string = pkg.version ?? "unknown";
const ILINK_APP_ID: string = pkg.ilink_appid ?? "";

export function buildClientVersion(version: string): number {
  const parts = version.split(".").map((p) => parseInt(p, 10));
  const major = parts[0] ?? 0;
  const minor = parts[1] ?? 0;
  const patch = parts[2] ?? 0;
  return ((major & 0xff) << 16) | ((minor & 0xff) << 8) | (patch & 0xff);
}

const ILINK_APP_CLIENT_VERSION: number = buildClientVersion(pkg.version ?? "0.0.0");

export function buildBaseInfo(): BaseInfo {
  return { channel_version: CHANNEL_VERSION };
}

export const DEFAULT_LONG_POLL_TIMEOUT_MS = 35_000;
export const DEFAULT_API_TIMEOUT_MS = 15_000;
export const DEFAULT_CONFIG_TIMEOUT_MS = 10_000;

function ensureTrailingSlash(url: string): string {
  return url.endsWith("/") ? url : `${url}/`;
}

function randomWechatUin(): string {
  const uint32 = crypto.randomBytes(4).readUInt32BE(0);
  return Buffer.from(String(uint32), "utf-8").toString("base64");
}

export function buildCommonHeaders(): Record<string, string> {
  return {
    "iLink-App-Id": ILINK_APP_ID,
    "iLink-App-ClientVersion": String(ILINK_APP_CLIENT_VERSION),
  };
}

export function buildHeaders(opts: { token?: string; body: string }): Record<string, string> {
  const headers: Record<string, string> = {
    "Content-Type": "application/json",
    AuthorizationType: "ilink_bot_token",
    "Content-Length": String(Buffer.byteLength(opts.body, "utf-8")),
    "X-WECHAT-UIN": randomWechatUin(),
    ...buildCommonHeaders(),
  };
  if (opts.token?.trim()) {
    headers.Authorization = `Bearer ${opts.token.trim()}`;
  }
  return headers;
}

export async function apiGetFetch(params: {
  baseUrl: string;
  endpoint: string;
  timeoutMs?: number;
  label: string;
}): Promise<string> {
  const base = ensureTrailingSlash(params.baseUrl);
  const url = new URL(params.endpoint, base);
  const hdrs = buildCommonHeaders();

  const timeoutMs = params.timeoutMs;
  const controller =
    timeoutMs != null && timeoutMs > 0 ? new AbortController() : undefined;
  const t =
    controller != null && timeoutMs != null
      ? setTimeout(() => controller.abort(), timeoutMs)
      : undefined;
  try {
    const res = await fetch(url.toString(), {
      method: "GET",
      headers: hdrs,
      ...(controller ? { signal: controller.signal } : {}),
    });
    if (t !== undefined) clearTimeout(t);
    const rawText = await res.text();
    if (!res.ok) {
      throw new Error(`${params.label} ${res.status}: ${rawText}`);
    }
    return rawText;
  } catch (err) {
    if (t !== undefined) clearTimeout(t);
    throw err;
  }
}

export async function apiPostFetch(params: {
  baseUrl: string;
  endpoint: string;
  body: string;
  token?: string;
  timeoutMs: number;
  label: string;
  signal?: AbortSignal;
}): Promise<string> {
  const base = ensureTrailingSlash(params.baseUrl);
  const url = new URL(params.endpoint, base);
  const hdrs = buildHeaders({
    ...(params.token !== undefined ? { token: params.token } : {}),
    body: params.body,
  });

  const controller = new AbortController();
  const t = setTimeout(() => controller.abort(), params.timeoutMs);
  const forward = () => controller.abort();
  if (params.signal) {
    if (params.signal.aborted) controller.abort();
    else params.signal.addEventListener("abort", forward, { once: true });
  }
  try {
    const res = await fetch(url.toString(), {
      method: "POST",
      headers: hdrs,
      body: params.body,
      signal: controller.signal,
    });
    const rawText = await res.text();
    if (!res.ok) {
      throw new Error(`${params.label} ${res.status}: ${rawText}`);
    }
    return rawText;
  } finally {
    clearTimeout(t);
    if (params.signal) params.signal.removeEventListener("abort", forward);
  }
}
