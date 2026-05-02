import test from "node:test";
import assert from "node:assert/strict";

import {
  pollDeviceToken,
  refreshUAT,
  requestDeviceAuthorization,
  resolveOAuthEndpoints,
} from "../dist/auth/device-flow.js";

const noopLogger = {
  debug() {},
  info() {},
  warn() {},
  error() {},
};

function fakeFetch(handler) {
  const calls = [];
  const f = async (url, init) => {
    calls.push({ url, init });
    const r = await handler(url, init, calls.length);
    if (r instanceof Error) throw r;
    return r;
  };
  return { fetch: f, calls };
}

function jsonResponse(body, ok = true, status = 200) {
  return {
    ok,
    status,
    async text() {
      return JSON.stringify(body);
    },
    async json() {
      return body;
    },
  };
}

test("resolveOAuthEndpoints maps the known feishu/lark hosts", () => {
  assert.deepEqual(resolveOAuthEndpoints("https://open.feishu.cn"), {
    deviceAuthorization:
      "https://accounts.feishu.cn/oauth/v1/device_authorization",
    token: "https://open.feishu.cn/open-apis/authen/v2/oauth/token",
  });
  assert.deepEqual(resolveOAuthEndpoints("https://open.larksuite.com/"), {
    deviceAuthorization:
      "https://accounts.larksuite.com/oauth/v1/device_authorization",
    token: "https://open.larksuite.com/open-apis/authen/v2/oauth/token",
  });
});

test("resolveOAuthEndpoints derives accounts.X from open.X for custom hosts", () => {
  // Private cloud / regional mirrors derive `accounts.` from `open.`.
  assert.deepEqual(resolveOAuthEndpoints("https://open.example.cn"), {
    deviceAuthorization:
      "https://accounts.example.cn/oauth/v1/device_authorization",
    token: "https://open.example.cn/open-apis/authen/v2/oauth/token",
  });
});

test("requestDeviceAuthorization sends Basic auth + scope and parses the response", async () => {
  const f = fakeFetch(() =>
    jsonResponse({
      device_code: "DC_123",
      user_code: "ABCD-1234",
      verification_uri: "https://accounts.feishu.cn/oauth/v1/device",
      verification_uri_complete:
        "https://accounts.feishu.cn/oauth/v1/device?user_code=ABCD-1234",
      expires_in: 240,
      interval: 5,
    }),
  );
  const r = await requestDeviceAuthorization({
    appId: "cli_xxx",
    appSecret: "sec_yyy",
    baseUrl: "https://open.feishu.cn",
    logger: noopLogger,
    fetchImpl: f.fetch,
    scope: "im:message",
  });
  assert.equal(r.deviceCode, "DC_123");
  assert.equal(r.userCode, "ABCD-1234");
  assert.equal(
    r.verificationUriComplete,
    "https://accounts.feishu.cn/oauth/v1/device?user_code=ABCD-1234",
  );
  assert.equal(r.interval, 5);

  const [call] = f.calls;
  assert.equal(
    call.url,
    "https://accounts.feishu.cn/oauth/v1/device_authorization",
  );
  // Basic auth = base64("cli_xxx:sec_yyy")
  assert.equal(
    call.init.headers["Authorization"],
    "Basic Y2xpX3h4eDpzZWNfeXl5",
  );
  // offline_access auto-appended so we always get a refresh_token back.
  const body = new URLSearchParams(call.init.body);
  assert.equal(body.get("client_id"), "cli_xxx");
  assert.equal(body.get("scope"), "im:message offline_access");
});

test("requestDeviceAuthorization throws on Lark-side error payload", async () => {
  const f = fakeFetch(() =>
    jsonResponse(
      { error: "invalid_client", error_description: "app_id mismatch" },
      false,
      400,
    ),
  );
  await assert.rejects(
    requestDeviceAuthorization({
      appId: "cli_xxx",
      appSecret: "wrong",
      baseUrl: "https://open.feishu.cn",
      logger: noopLogger,
      fetchImpl: f.fetch,
    }),
    /app_id mismatch/,
  );
});

test("pollDeviceToken keeps polling on authorization_pending then yields token", async () => {
  const f = fakeFetch((_url, _init, n) => {
    if (n === 1) return jsonResponse({ error: "authorization_pending" });
    if (n === 2) return jsonResponse({ error: "authorization_pending" });
    return jsonResponse({
      access_token: "uat_v2_xxx",
      refresh_token: "ur_v2_yyy",
      expires_in: 7200,
      refresh_token_expires_in: 604800,
      scope: "im:message offline_access",
    });
  });
  const r = await pollDeviceToken({
    appId: "cli_xxx",
    appSecret: "sec_yyy",
    baseUrl: "https://open.feishu.cn",
    logger: noopLogger,
    fetchImpl: f.fetch,
    deviceCode: "DC_123",
    interval: 0, // fast polling for tests
    expiresIn: 240,
  });
  assert.equal(r.ok, true);
  assert.equal(r.token.accessToken, "uat_v2_xxx");
  assert.equal(r.token.refreshToken, "ur_v2_yyy");
  assert.equal(r.token.expiresIn, 7200);
  assert.equal(f.calls.length, 3);
});

test("pollDeviceToken returns access_denied terminally when user rejects", async () => {
  const f = fakeFetch(() => jsonResponse({ error: "access_denied" }));
  const r = await pollDeviceToken({
    appId: "x",
    appSecret: "y",
    baseUrl: "https://open.feishu.cn",
    logger: noopLogger,
    fetchImpl: f.fetch,
    deviceCode: "DC_x",
    interval: 0,
    expiresIn: 240,
  });
  assert.equal(r.ok, false);
  assert.equal(r.error, "access_denied");
  assert.match(r.message, /denied/);
  // One terminal poll, no retries on access_denied.
  assert.equal(f.calls.length, 1);
});

test("pollDeviceToken backs off on slow_down", async () => {
  const f = fakeFetch((_url, _init, n) => {
    if (n === 1) return jsonResponse({ error: "slow_down" });
    return jsonResponse({
      access_token: "uat",
      refresh_token: "ur",
      expires_in: 7200,
      scope: "",
    });
  });
  const r = await pollDeviceToken({
    appId: "x",
    appSecret: "y",
    baseUrl: "https://open.feishu.cn",
    logger: noopLogger,
    fetchImpl: f.fetch,
    deviceCode: "DC_x",
    interval: 0,
    expiresIn: 240,
  });
  assert.equal(r.ok, true);
  assert.equal(f.calls.length, 2);
});

test("pollDeviceToken honours AbortSignal", async () => {
  // Long interval; abort immediately so we land in the sleep promise.
  const ctrl = new AbortController();
  const f = fakeFetch(() => jsonResponse({ error: "authorization_pending" }));
  const promise = pollDeviceToken({
    appId: "x",
    appSecret: "y",
    baseUrl: "https://open.feishu.cn",
    logger: noopLogger,
    fetchImpl: f.fetch,
    deviceCode: "DC_x",
    interval: 60,
    expiresIn: 240,
    signal: ctrl.signal,
  });
  ctrl.abort();
  const r = await promise;
  assert.equal(r.ok, false);
  assert.equal(r.error, "expired_token");
  // No fetch fired — abort hit the sleep before the first call.
  assert.equal(f.calls.length, 0);
});

test("refreshUAT exchanges a refresh_token for a fresh bundle and rotates", async () => {
  const f = fakeFetch(() =>
    jsonResponse({
      access_token: "uat_new",
      refresh_token: "ur_rotated",
      expires_in: 7200,
      refresh_token_expires_in: 604800,
      scope: "im:message offline_access",
    }),
  );
  const r = await refreshUAT({
    appId: "cli_xxx",
    appSecret: "sec_yyy",
    baseUrl: "https://open.feishu.cn",
    logger: noopLogger,
    fetchImpl: f.fetch,
    refreshToken: "ur_old",
  });
  assert.equal(r.ok, true);
  assert.equal(r.token.accessToken, "uat_new");
  // OAuth 2.1: refresh tokens rotate. Make sure we picked up the new one.
  assert.equal(r.token.refreshToken, "ur_rotated");
  const body = new URLSearchParams(f.calls[0].init.body);
  assert.equal(body.get("grant_type"), "refresh_token");
  assert.equal(body.get("refresh_token"), "ur_old");
});

test("refreshUAT preserves the old refresh_token when Lark omits a rotation", async () => {
  // Some Lark deployments don't rotate the refresh_token on every
  // refresh. Keep the old one so we can refresh again later.
  const f = fakeFetch(() =>
    jsonResponse({
      access_token: "uat_new",
      expires_in: 7200,
    }),
  );
  const r = await refreshUAT({
    appId: "x",
    appSecret: "y",
    baseUrl: "https://open.feishu.cn",
    logger: noopLogger,
    fetchImpl: f.fetch,
    refreshToken: "ur_keep_me",
  });
  assert.equal(r.ok, true);
  assert.equal(r.token.refreshToken, "ur_keep_me");
});

test("refreshUAT surfaces invalid_grant as terminal expired_token", async () => {
  const f = fakeFetch(() =>
    jsonResponse({
      error: "invalid_grant",
      error_description: "refresh_token expired",
    }),
  );
  const r = await refreshUAT({
    appId: "x",
    appSecret: "y",
    baseUrl: "https://open.feishu.cn",
    logger: noopLogger,
    fetchImpl: f.fetch,
    refreshToken: "ur_dead",
  });
  assert.equal(r.ok, false);
  assert.equal(r.error, "expired_token");
  assert.match(r.message, /expired/);
});

test("refreshUAT classifies transient OAuth errors as network_error so the UAT survives Lark hiccups (Codex review #4)", async () => {
  // Critical regression: previously every non-success OAuth error
  // got classified as expired_token, and the scheduler/accessor
  // delete UATs on expired_token. So a Feishu OAuth tier outage
  // (server_error, temporarily_unavailable) wiped every refresh-due
  // UAT and forced mass reauth. Verify the new classification keeps
  // these classified as network_error so the caller retains the UAT.
  const transientCodes = [
    "server_error",
    "temporarily_unavailable",
    "rate_limited",
    "unknown_garbage_code", // unknown codes treated as transient
  ];
  for (const code of transientCodes) {
    const f = fakeFetch(() => jsonResponse({ error: code }));
    const r = await refreshUAT({
      appId: "x",
      appSecret: "y",
      baseUrl: "https://open.feishu.cn",
      logger: noopLogger,
      fetchImpl: f.fetch,
      refreshToken: "ur_alive",
    });
    assert.equal(r.ok, false);
    assert.equal(
      r.error,
      "network_error",
      `${code}: must classify as network_error, not expired_token`,
    );
  }
});

test("refreshUAT keeps the full set of terminal grant errors mapped to expired_token", async () => {
  // Per RFC 6749 §5.2, these all mean "this refresh token is dead,
  // get a new grant" — drop the UAT.
  const terminalCodes = [
    "invalid_grant",
    "invalid_client",
    "unauthorized_client",
    "unsupported_grant_type",
    "invalid_scope",
    "access_denied",
  ];
  for (const code of terminalCodes) {
    const f = fakeFetch(() => jsonResponse({ error: code }));
    const r = await refreshUAT({
      appId: "x",
      appSecret: "y",
      baseUrl: "https://open.feishu.cn",
      logger: noopLogger,
      fetchImpl: f.fetch,
      refreshToken: "ur",
    });
    assert.equal(r.ok, false);
    assert.equal(
      r.error,
      "expired_token",
      `${code}: must classify as expired_token (terminal grant error)`,
    );
  }
});
