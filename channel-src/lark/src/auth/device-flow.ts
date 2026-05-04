import type { Logger } from "@aura/channel-sdk";

/** RFC 8628 Device Authorization Grant client for Lark / Feishu.
 *
 * Lark exposes the standard OAuth 2.0 Device Authorization endpoints —
 * `accounts.feishu.cn/oauth/v1/device_authorization` for the initial
 * code request, `open.feishu.cn/open-apis/authen/v2/oauth/token` for
 * polling. This sidesteps the need for any callback URL: the sidecar
 * is the OAuth client, Lark hosts the verification page, the user
 * authorises in their Lark app/browser, and the sidecar polls until
 * tokens land. The whole flow works behind a NAT with no public
 * ingress.
 *
 * The refresh path uses the same `/oauth/token` endpoint with
 * `grant_type=refresh_token`; we expose it here too because all the
 * endpoint resolution + error handling already lives in this module. */

export interface DeviceAuthResponse {
  deviceCode: string;
  userCode: string;
  /** Lark's hosted verification page (user types `userCode` here). */
  verificationUri: string;
  /** Same page with the code pre-filled — what the chat card links to. */
  verificationUriComplete: string;
  /** Seconds the device_code stays valid (typically 240s). */
  expiresIn: number;
  /** Recommended polling interval in seconds (typically 5s). */
  interval: number;
}

export interface DeviceFlowToken {
  accessToken: string;
  refreshToken: string;
  /** Seconds until access_token expires (Lark default 7200). */
  expiresIn: number;
  /** Seconds until refresh_token expires (Lark default 604800 = 7 days). */
  refreshExpiresIn: number;
  scope: string;
}

export type DeviceFlowError =
  | "authorization_pending"
  | "slow_down"
  | "access_denied"
  | "expired_token"
  /** Transient — caller should retry. Distinguished from `expired_token`
   * so refresh callers don't delete a perfectly valid UAT just because
   * the network blipped. */
  | "network_error";

export type DeviceFlowResult =
  | { ok: true; token: DeviceFlowToken }
  | { ok: false; error: DeviceFlowError; message: string };

export interface OAuthEndpoints {
  deviceAuthorization: string;
  token: string;
}

/** Resolve OAuth endpoint URLs from a Lark base URL.
 *
 * The two well-known mainstream hosts get hard-coded mappings. Other
 * hosts (private cloud, regional mirrors) are derived by the same
 * `open.X → accounts.X` substitution openclaw uses, with a graceful
 * fallback to the base URL itself if the URL doesn't parse. */
export function resolveOAuthEndpoints(baseUrl: string): OAuthEndpoints {
  const normalised = baseUrl.replace(/\/+$/, "");
  if (normalised === "https://open.feishu.cn") {
    return {
      deviceAuthorization: "https://accounts.feishu.cn/oauth/v1/device_authorization",
      token: "https://open.feishu.cn/open-apis/authen/v2/oauth/token",
    };
  }
  if (normalised === "https://open.larksuite.com") {
    return {
      deviceAuthorization: "https://accounts.larksuite.com/oauth/v1/device_authorization",
      token: "https://open.larksuite.com/open-apis/authen/v2/oauth/token",
    };
  }
  let accountsBase = normalised;
  try {
    const parsed = new URL(normalised);
    if (parsed.hostname.startsWith("open.")) {
      accountsBase = `${parsed.protocol}//${parsed.hostname.replace(/^open\./, "accounts.")}`;
    }
  } catch {
    /* fall through — accountsBase stays at normalised */
  }
  return {
    deviceAuthorization: `${accountsBase}/oauth/v1/device_authorization`,
    token: `${normalised}/open-apis/authen/v2/oauth/token`,
  };
}

export interface DeviceFlowOptions {
  appId: string;
  appSecret: string;
  baseUrl: string;
  logger: Logger;
  /** Override fetch for tests. Defaults to global `fetch`. */
  fetchImpl?: typeof fetch;
  /** OAuth scope string. `offline_access` is appended automatically so
   * the response always carries a refresh_token. */
  scope?: string;
}

/** Step 1 — request a device_code from Lark.
 *
 * Confidential-client auth (HTTP Basic with `appId:appSecret`) plus
 * `offline_access` appended to the scope so we always get a refresh
 * token back from step 2. */
export async function requestDeviceAuthorization(
  opts: DeviceFlowOptions,
): Promise<DeviceAuthResponse> {
  const endpoints = resolveOAuthEndpoints(opts.baseUrl);
  const scope = ensureOfflineAccess(opts.scope);
  const basicAuth = Buffer.from(`${opts.appId}:${opts.appSecret}`).toString(
    "base64",
  );
  const body = new URLSearchParams({ client_id: opts.appId, scope });
  const f = opts.fetchImpl ?? fetch;
  opts.logger.debug(
    `lark device_authorization → ${endpoints.deviceAuthorization} scope="${scope}"`,
  );
  const resp = await f(endpoints.deviceAuthorization, {
    method: "POST",
    headers: {
      "Content-Type": "application/x-www-form-urlencoded",
      Authorization: `Basic ${basicAuth}`,
    },
    body: body.toString(),
  });
  const text = await resp.text();
  let data: Record<string, unknown>;
  try {
    data = JSON.parse(text) as Record<string, unknown>;
  } catch {
    throw new Error(
      `device_authorization: HTTP ${resp.status} returned non-JSON: ${text.slice(0, 200)}`,
    );
  }
  if (!resp.ok || data.error !== undefined) {
    const msg =
      typeof data.error_description === "string"
        ? data.error_description
        : typeof data.error === "string"
          ? data.error
          : `HTTP ${resp.status}`;
    throw new Error(`device_authorization rejected: ${msg}`);
  }
  return {
    deviceCode: requireString(data, "device_code"),
    userCode: requireString(data, "user_code"),
    verificationUri: requireString(data, "verification_uri"),
    verificationUriComplete:
      typeof data.verification_uri_complete === "string"
        ? data.verification_uri_complete
        : requireString(data, "verification_uri"),
    expiresIn: typeof data.expires_in === "number" ? data.expires_in : 240,
    interval: typeof data.interval === "number" ? data.interval : 5,
  };
}

/** Step 2 — poll the token endpoint until the user approves, denies,
 * or the device_code expires.
 *
 * Honours `slow_down` (back off by +5s, capped) and `authorization_pending`
 * (keep polling). Aborts cleanly on the supplied `signal`. */
export async function pollDeviceToken(
  opts: DeviceFlowOptions & {
    deviceCode: string;
    interval: number;
    expiresIn: number;
    signal?: AbortSignal;
  },
): Promise<DeviceFlowResult> {
  const MAX_INTERVAL = 60;
  const MAX_ATTEMPTS = 200;
  const endpoints = resolveOAuthEndpoints(opts.baseUrl);
  const f = opts.fetchImpl ?? fetch;
  const deadline = Date.now() + opts.expiresIn * 1000;
  let interval = opts.interval;
  let attempts = 0;

  while (Date.now() < deadline && attempts < MAX_ATTEMPTS) {
    attempts++;
    if (opts.signal?.aborted) {
      return { ok: false, error: "expired_token", message: "polling cancelled" };
    }
    try {
      await sleep(interval * 1000, opts.signal);
    } catch {
      return { ok: false, error: "expired_token", message: "polling cancelled" };
    }

    let data: Record<string, unknown>;
    try {
      const resp = await f(endpoints.token, {
        method: "POST",
        headers: { "Content-Type": "application/x-www-form-urlencoded" },
        body: new URLSearchParams({
          grant_type: "urn:ietf:params:oauth:grant-type:device_code",
          device_code: opts.deviceCode,
          client_id: opts.appId,
          client_secret: opts.appSecret,
        }).toString(),
      });
      data = (await resp.json()) as Record<string, unknown>;
    } catch (err) {
      opts.logger.debug(`lark token poll network error: ${String(err)}`);
      interval = Math.min(interval + 1, MAX_INTERVAL);
      continue;
    }

    const error = typeof data.error === "string" ? data.error : undefined;
    if (!error && typeof data.access_token === "string") {
      const refreshToken =
        typeof data.refresh_token === "string" ? data.refresh_token : "";
      const accessExpires =
        typeof data.expires_in === "number" ? data.expires_in : 7200;
      const refreshExpires = refreshToken
        ? typeof data.refresh_token_expires_in === "number"
          ? data.refresh_token_expires_in
          : 604800
        : accessExpires;
      return {
        ok: true,
        token: {
          accessToken: data.access_token,
          refreshToken,
          expiresIn: accessExpires,
          refreshExpiresIn: refreshExpires,
          scope: typeof data.scope === "string" ? data.scope : "",
        },
      };
    }

    if (error === "authorization_pending") continue;
    if (error === "slow_down") {
      interval = Math.min(interval + 5, MAX_INTERVAL);
      continue;
    }
    if (error === "access_denied") {
      return { ok: false, error: "access_denied", message: "user denied authorization" };
    }
    if (error === "expired_token" || error === "invalid_grant") {
      return { ok: false, error: "expired_token", message: "device code expired or invalid" };
    }
    const desc =
      typeof data.error_description === "string"
        ? data.error_description
        : (error ?? "unknown error");
    return { ok: false, error: "expired_token", message: desc };
  }

  return { ok: false, error: "expired_token", message: "device code expired before user approved" };
}

/** Refresh an existing UAT.
 *
 * Lark's refresh response has the same shape as the device_code grant
 * response (and rotates the refresh_token, per OAuth 2.1). Returns
 * the new token bundle on success or a `DeviceFlowResult` with
 * `ok: false` on terminal failure (refresh_token expired / revoked). */
export async function refreshUAT(
  opts: DeviceFlowOptions & { refreshToken: string },
): Promise<DeviceFlowResult> {
  const endpoints = resolveOAuthEndpoints(opts.baseUrl);
  const f = opts.fetchImpl ?? fetch;
  let data: Record<string, unknown>;
  try {
    const resp = await f(endpoints.token, {
      method: "POST",
      headers: { "Content-Type": "application/x-www-form-urlencoded" },
      body: new URLSearchParams({
        grant_type: "refresh_token",
        refresh_token: opts.refreshToken,
        client_id: opts.appId,
        client_secret: opts.appSecret,
      }).toString(),
    });
    data = (await resp.json()) as Record<string, unknown>;
  } catch (err) {
    return {
      ok: false,
      error: "network_error",
      message: `refresh network error: ${String(err)}`,
    };
  }
  const error = typeof data.error === "string" ? data.error : undefined;
  if (!error && typeof data.access_token === "string") {
    const refreshToken =
      typeof data.refresh_token === "string"
        ? data.refresh_token
        : opts.refreshToken;
    const accessExpires =
      typeof data.expires_in === "number" ? data.expires_in : 7200;
    const refreshExpires =
      typeof data.refresh_token_expires_in === "number"
        ? data.refresh_token_expires_in
        : 604800;
    return {
      ok: true,
      token: {
        accessToken: data.access_token,
        refreshToken,
        expiresIn: accessExpires,
        refreshExpiresIn: refreshExpires,
        scope: typeof data.scope === "string" ? data.scope : "",
      },
    };
  }
  // Codex review #4: classify the OAuth error so the caller can
  // distinguish terminal-grant errors (drop the UAT) from transient
  // server-side errors (keep the UAT, retry next sweep). The
  // previous unconditional `expired_token` mapping turned a Lark
  // outage into mass UAT deletion + reauth storm.
  const message =
    typeof data.error_description === "string"
      ? data.error_description
      : (error ?? "refresh failed");
  if (isTransientOAuthError(error)) {
    return { ok: false, error: "network_error", message };
  }
  return { ok: false, error: "expired_token", message };
}

/** Per RFC 6749 §5.2 + RFC 6750 §3.1, terminal grant errors mean
 * "this refresh token will never work again" — drop the UAT.
 * Everything else (server_error, temporarily_unavailable, unknown
 * codes, malformed payloads) is treated as transient: keep the UAT
 * around so the next sweep retries instead of forcing the user to
 * reauth because Lark's OAuth tier hiccupped. */
const TERMINAL_GRANT_ERRORS = new Set<string>([
  "invalid_grant",
  "invalid_client",
  "unauthorized_client",
  "unsupported_grant_type",
  "invalid_scope",
  "access_denied",
]);

function isTransientOAuthError(error: string | undefined): boolean {
  if (!error) return true; // missing error code → can't tell, treat conservatively
  return !TERMINAL_GRANT_ERRORS.has(error);
}

/** Look up the OAuth subject (the actual `open_id` Lark issued the
 * token to) by calling `/authen/v1/user_info` with the freshly-minted
 * UAT. Used by the auth flow to verify the token belongs to the user
 * we asked to authorise — without this check, a group-chat member who
 * isn't the original requester could click the auth link and end up
 * with their UAT silently stored under the requester's user id. */
export interface UATSubject {
  openId: string;
  unionId?: string;
  userId?: string;
  name?: string;
}

export type UATSubjectResult =
  | { ok: true; subject: UATSubject }
  | { ok: false; reason: string };

export async function fetchUATSubject(opts: {
  accessToken: string;
  baseUrl: string;
  logger: Logger;
  fetchImpl?: typeof fetch;
}): Promise<UATSubjectResult> {
  const f = opts.fetchImpl ?? fetch;
  const url = `${opts.baseUrl.replace(/\/+$/, "")}/open-apis/authen/v1/user_info`;
  let resp: Response;
  try {
    resp = await f(url, {
      headers: { Authorization: `Bearer ${opts.accessToken}` },
    });
  } catch (err) {
    return { ok: false, reason: `userInfo network error: ${String(err)}` };
  }
  let data: Record<string, unknown>;
  try {
    data = (await resp.json()) as Record<string, unknown>;
  } catch {
    return { ok: false, reason: `userInfo returned non-JSON HTTP ${resp.status}` };
  }
  const code = typeof data.code === "number" ? data.code : -1;
  if (!resp.ok || (code !== 0 && code !== -1)) {
    const msg = typeof data.msg === "string" ? data.msg : `HTTP ${resp.status}`;
    return { ok: false, reason: `userInfo rejected: ${msg}` };
  }
  const inner = (data.data ?? data) as Record<string, unknown>;
  const openId = typeof inner.open_id === "string" ? inner.open_id : undefined;
  if (!openId) {
    return { ok: false, reason: "userInfo response missing open_id" };
  }
  const subject: UATSubject = { openId };
  if (typeof inner.union_id === "string") subject.unionId = inner.union_id;
  if (typeof inner.user_id === "string") subject.userId = inner.user_id;
  if (typeof inner.name === "string") subject.name = inner.name;
  return { ok: true, subject };
}

function ensureOfflineAccess(scope: string | undefined): string {
  const base = (scope ?? "").trim();
  if (base.length === 0) return "offline_access";
  return base.split(/\s+/).includes("offline_access")
    ? base
    : `${base} offline_access`;
}

function requireString(
  data: Record<string, unknown>,
  key: string,
): string {
  const v = data[key];
  if (typeof v !== "string" || v.length === 0) {
    throw new Error(`device_authorization response missing string field: ${key}`);
  }
  return v;
}

function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    const timer = setTimeout(resolve, ms);
    signal?.addEventListener(
      "abort",
      () => {
        clearTimeout(timer);
        reject(new Error("aborted"));
      },
      { once: true },
    );
  });
}
