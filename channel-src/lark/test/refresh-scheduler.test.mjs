import test from "node:test";
import assert from "node:assert/strict";

import { UATStore } from "../dist/auth/uat-store.js";
import { UATRefreshScheduler } from "../dist/auth/refresh-scheduler.js";
import { UATMutex } from "../dist/auth/auto-auth.js";

const noopLogger = {
  debug() {},
  info() {},
  warn() {},
  error() {},
};

class MemoryScope {
  constructor() {
    this.map = new Map();
  }
  async get(key) {
    return this.map.has(key) ? this.map.get(key) : null;
  }
  async set(key, value) {
    this.map.set(key, value);
  }
  async delete(key) {
    this.map.delete(key);
  }
  async list() {
    return [...this.map.keys()];
  }
}

function jsonResponse(body, ok = true) {
  return {
    ok,
    status: ok ? 200 : 400,
    async text() {
      return JSON.stringify(body);
    },
    async json() {
      return body;
    },
  };
}

function fakeFetch(handler) {
  const calls = [];
  return {
    calls,
    fetch: async (url, init) => {
      calls.push({ url, init });
      return handler(url, init, calls.length);
    },
  };
}

function buildScheduler(opts = {}) {
  const scope = new MemoryScope();
  const store = new UATStore(scope);
  const f = opts.fetch ?? fakeFetch(() => jsonResponse({}));
  const scheduler = new UATRefreshScheduler({
    store,
    mutex: new UATMutex(),
    appId: "cli_xxx",
    appSecret: "sec_yyy",
    baseUrl: "https://open.feishu.cn",
    logger: noopLogger,
    fetchImpl: f.fetch,
    sweepIntervalMs: 50,
    refreshAheadMs: 60_000,
    ...opts.schedulerOverrides,
  });
  return { scope, store, scheduler, fakeFetch: f };
}

test("UATRefreshScheduler skips users not yet inside the refresh window", async () => {
  const { store, scheduler, fakeFetch: f } = buildScheduler();
  // Fresh UAT — expires in 2h, well outside the 60s ahead window.
  await store.set("ou_alice", {
    accessToken: "uat_old",
    refreshToken: "ur_old",
    expiresAt: Date.now() + 7200_000,
    refreshExpiresAt: Date.now() + 604800_000,
    scope: "im:message offline_access",
    grantedAt: Date.now() - 60_000,
  });

  const r = await scheduler.sweepOnce();
  assert.equal(r.scanned, 1);
  assert.deepEqual(r.refreshed, []);
  assert.deepEqual(r.expired, []);
  // No HTTP — the scheduler short-circuited before the refresh call.
  assert.equal(f.calls.length, 0);
});

test("UATRefreshScheduler refreshes UATs inside the refresh window", async () => {
  const f = fakeFetch(() =>
    jsonResponse({
      access_token: "uat_new",
      refresh_token: "ur_new",
      expires_in: 7200,
      refresh_token_expires_in: 604800,
      scope: "im:message offline_access",
    }),
  );
  const { store, scheduler } = buildScheduler({ fetch: f });
  const grantedAt = Date.now() - 7100_000;
  await store.set("ou_alice", {
    accessToken: "uat_old",
    refreshToken: "ur_old",
    expiresAt: Date.now() + 30_000, // 30s left → inside the 60s window
    refreshExpiresAt: Date.now() + 604800_000,
    scope: "im:message offline_access",
    grantedAt,
  });

  const r = await scheduler.sweepOnce();
  assert.deepEqual(r.refreshed, ["ou_alice"]);
  assert.deepEqual(r.expired, []);

  // Stored token rotated; grantedAt preserved across refresh so the
  // auth card's "Authorized at" subtitle stays stable.
  const after = await store.get("ou_alice");
  assert.equal(after.accessToken, "uat_new");
  assert.equal(after.refreshToken, "ur_new");
  assert.equal(after.grantedAt, grantedAt);
});

test("UATRefreshScheduler deletes the UAT when refresh_token is already expired", async () => {
  const { store, scheduler, fakeFetch: f } = buildScheduler();
  await store.set("ou_alice", {
    accessToken: "uat_x",
    refreshToken: "ur_dead",
    expiresAt: Date.now() - 1000,
    refreshExpiresAt: Date.now() - 1000, // already past
    scope: "",
    grantedAt: Date.now() - 86400_000,
  });

  const r = await scheduler.sweepOnce();
  assert.deepEqual(r.expired, ["ou_alice"]);
  assert.equal(await store.get("ou_alice"), null);
  // No fetch — we noticed the dead refresh_token before calling Lark.
  assert.equal(f.calls.length, 0);
});

test("UATRefreshScheduler deletes the UAT when Lark says invalid_grant on refresh", async () => {
  const f = fakeFetch(() =>
    jsonResponse({ error: "invalid_grant", error_description: "refresh expired" }),
  );
  const { store, scheduler } = buildScheduler({ fetch: f });
  await store.set("ou_alice", {
    accessToken: "uat_x",
    refreshToken: "ur_dead_serverside",
    expiresAt: Date.now() + 30_000,
    refreshExpiresAt: Date.now() + 604800_000, // client thinks it's alive
    scope: "",
    grantedAt: Date.now() - 1000,
  });

  const r = await scheduler.sweepOnce();
  assert.deepEqual(r.expired, ["ou_alice"]);
  assert.equal(await store.get("ou_alice"), null);
});

test("UATRefreshScheduler treats a transient network error as a non-terminal error", async () => {
  // Network error → keep the UAT around so the next sweep retries.
  const f = fakeFetch(async () => {
    throw new Error("ECONNRESET");
  });
  const { store, scheduler } = buildScheduler({ fetch: f });
  await store.set("ou_alice", {
    accessToken: "uat_x",
    refreshToken: "ur_alive",
    expiresAt: Date.now() + 30_000,
    refreshExpiresAt: Date.now() + 604800_000,
    scope: "",
    grantedAt: Date.now(),
  });

  const r = await scheduler.sweepOnce();
  assert.equal(r.errors.length, 1);
  assert.equal(r.errors[0].userOpenId, "ou_alice");
  // Still in the store — next sweep retries.
  const after = await store.get("ou_alice");
  assert.equal(after.refreshToken, "ur_alive");
});

test("UATRefreshScheduler keeps the UAT when Lark returns a transient OAuth server_error (Codex review #4)", async () => {
  // The dangerous regression Codex flagged: previously every non-
  // success OAuth response (server_error, temporarily_unavailable,
  // unknown codes) was classified as expired_token, and the
  // scheduler deleted UATs on expired_token. So a Lark OAuth tier
  // outage caused a mass UAT wipe — every refresh-due UAT got
  // deleted across all users at once. After the fix the OAuth
  // server_error must classify as transient and leave the UAT in
  // place for the next sweep.
  const f = fakeFetch(() =>
    jsonResponse({
      error: "server_error",
      error_description: "internal error",
    }),
  );
  const { store, scheduler } = buildScheduler({ fetch: f });
  await store.set("ou_alice", {
    accessToken: "uat_x",
    refreshToken: "ur_alive",
    expiresAt: Date.now() + 30_000,
    refreshExpiresAt: Date.now() + 604800_000,
    scope: "",
    grantedAt: Date.now(),
  });

  const r = await scheduler.sweepOnce();
  // Classified as a transient error (kept in store), NOT expired.
  assert.deepEqual(r.expired, []);
  assert.equal(r.errors.length, 1);
  assert.equal(r.errors[0].userOpenId, "ou_alice");
  // UAT survives — would have been mass-wiped under the old behavior.
  const after = await store.get("ou_alice");
  assert.equal(after.refreshToken, "ur_alive");
});

test("UATRefreshScheduler.sweepOnce is reentrant-safe", async () => {
  const f = fakeFetch(() =>
    jsonResponse({
      access_token: "uat_new",
      refresh_token: "ur_new",
      expires_in: 7200,
    }),
  );
  const { store, scheduler } = buildScheduler({ fetch: f });
  await store.set("ou_alice", {
    accessToken: "uat_x",
    refreshToken: "ur_alive",
    expiresAt: Date.now() + 30_000,
    refreshExpiresAt: Date.now() + 604800_000,
    scope: "",
    grantedAt: Date.now(),
  });

  // Two simultaneous sweepOnce calls → second short-circuits.
  const [a, b] = await Promise.all([scheduler.sweepOnce(), scheduler.sweepOnce()]);
  // Exactly one of the two did the work.
  const totalRefreshed = a.refreshed.length + b.refreshed.length;
  assert.equal(totalRefreshed, 1);
  // Lark called exactly once — no double refresh attempt.
  assert.equal(f.calls.length, 1);
});

test("UATRefreshScheduler.start fires automatic sweeps on the configured cadence", async () => {
  const f = fakeFetch(() =>
    jsonResponse({
      access_token: "uat_new",
      refresh_token: "ur_new",
      expires_in: 7200,
    }),
  );
  const { store, scheduler } = buildScheduler({
    fetch: f,
    schedulerOverrides: { sweepIntervalMs: 20, refreshAheadMs: 60_000 },
  });
  await store.set("ou_alice", {
    accessToken: "uat_x",
    refreshToken: "ur_alive",
    expiresAt: Date.now() + 30_000,
    refreshExpiresAt: Date.now() + 604800_000,
    scope: "",
    grantedAt: Date.now(),
  });

  scheduler.start();
  // Wait long enough for at least one sweep tick.
  await new Promise((r) => setTimeout(r, 80));
  await scheduler.stop();

  // At least one refresh happened.
  assert.ok(f.calls.length >= 1);
  const after = await store.get("ou_alice");
  assert.equal(after.accessToken, "uat_new");
});

test("UATRefreshScheduler skips users without refresh_token (no-op, kept in store)", async () => {
  const { store, scheduler, fakeFetch: f } = buildScheduler();
  await store.set("ou_alice", {
    accessToken: "uat_no_refresh",
    refreshToken: "", // some Lark deployments don't issue one
    expiresAt: Date.now() + 30_000,
    refreshExpiresAt: Date.now() + 30_000,
    scope: "",
    grantedAt: Date.now(),
  });

  const r = await scheduler.sweepOnce();
  assert.deepEqual(r.refreshed, []);
  assert.deepEqual(r.expired, []);
  assert.equal(f.calls.length, 0);
  // Still there — the access-token-expired tool call will trigger
  // a fresh device flow, which is the right outcome.
  assert.notEqual(await store.get("ou_alice"), null);
});
