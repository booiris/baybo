import test from "node:test";
import assert from "node:assert/strict";

import { AuthFlowController } from "../dist/auth/auth-flow.js";
import { UATAccessor, UATMutex, UAT_DEAD_CODES } from "../dist/auth/auto-auth.js";
import { UATStore } from "../dist/auth/uat-store.js";

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

function fakeChannel() {
  const sent = [];
  const updated = [];
  let nextId = 1;
  return {
    sent,
    updated,
    async send(chatId, input) {
      const messageId = `om_${nextId++}`;
      sent.push({ chatId, input, messageId });
      return { messageId };
    },
    async updateCard(messageId, card) {
      updated.push({ messageId, card });
    },
  };
}

function jsonResponse(body) {
  return {
    ok: true,
    status: 200,
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

const DEVICE_AUTH_BODY = {
  device_code: "DC_x",
  user_code: "ABCD-1234",
  verification_uri: "https://accounts.feishu.cn/oauth/v1/device",
  verification_uri_complete:
    "https://accounts.feishu.cn/oauth/v1/device?user_code=ABCD-1234",
  expires_in: 240,
  interval: 0,
};

function buildAccessor(handler) {
  const scope = new MemoryScope();
  const store = new UATStore(scope);
  const channel = fakeChannel();
  // Wrap so userInfo subject-verification calls (Codex review #1)
  // auto-resolve to ou_alice — every test that exercises the auth
  // flow uses ou_alice as the conversing user. Tests for subject
  // mismatch live in auth-flow.test.mjs; here we just need the
  // verification to succeed so the auto-auth interceptor logic
  // under test is what the assertions cover.
  let nonUserInfoCount = 0;
  const fetchHandler = async (url, init, _ignored) => {
    if (url.includes("/authen/v1/user_info")) {
      return {
        ok: true,
        status: 200,
        async text() {
          return JSON.stringify({ code: 0, data: { open_id: "ou_alice" } });
        },
        async json() {
          return { code: 0, data: { open_id: "ou_alice" } };
        },
      };
    }
    nonUserInfoCount++;
    return handler(url, init, nonUserInfoCount);
  };
  const f = fakeFetch(fetchHandler);
  const mutex = new UATMutex();
  const authFlow = new AuthFlowController({
    channel,
    store,
    appId: "x",
    appSecret: "y",
    baseUrl: "https://open.feishu.cn",
    logger: noopLogger,
    fetchImpl: f.fetch,
  });
  const accessor = new UATAccessor({
    store,
    authFlow,
    mutex,
    appId: "x",
    appSecret: "y",
    baseUrl: "https://open.feishu.cn",
    logger: noopLogger,
    fetchImpl: f.fetch,
  });
  return { accessor, store, channel, mutex, authFlow, fakeFetch: f };
}

test("UATMutex: serialises calls for the same user", async () => {
  const mutex = new UATMutex();
  const order = [];
  const a = mutex.withLock("ou_alice", async () => {
    order.push("a-start");
    await new Promise((r) => setTimeout(r, 5));
    order.push("a-end");
  });
  const b = mutex.withLock("ou_alice", async () => {
    order.push("b-start");
    order.push("b-end");
  });
  await Promise.all([a, b]);
  assert.deepEqual(order, ["a-start", "a-end", "b-start", "b-end"]);
});

test("UATMutex: independent users run concurrently", async () => {
  const mutex = new UATMutex();
  let aliceInside = false;
  let bobSawAliceInside = false;

  const alice = mutex.withLock("ou_alice", async () => {
    aliceInside = true;
    await new Promise((r) => setTimeout(r, 10));
    aliceInside = false;
  });
  // Yield once so alice's lock is established.
  await new Promise((r) => setImmediate(r));
  const bob = mutex.withLock("ou_bob", async () => {
    bobSawAliceInside = aliceInside;
  });
  await Promise.all([alice, bob]);
  assert.equal(bobSawAliceInside, true);
});

test("UATMutex: lock released even if fn throws", async () => {
  const mutex = new UATMutex();
  await assert.rejects(
    mutex.withLock("ou_alice", async () => {
      throw new Error("boom");
    }),
    /boom/,
  );
  // Subsequent acquire works (would hang if lock leaked).
  let ran = false;
  await mutex.withLock("ou_alice", async () => {
    ran = true;
  });
  assert.equal(ran, true);
});

test("UATAccessor.invoke: uses cached UAT when fresh, no fetches", async () => {
  const { accessor, store, fakeFetch: f } = buildAccessor(() =>
    jsonResponse({ access_token: "should_never_be_called" }),
  );
  await store.set("ou_alice", {
    accessToken: "uat_cached",
    refreshToken: "ur",
    expiresAt: Date.now() + 7200_000,
    refreshExpiresAt: Date.now() + 604800_000,
    scope: "",
    grantedAt: Date.now(),
  });

  let receivedUAT;
  const r = await accessor.invoke(
    {
      userOpenId: "ou_alice",
      chatId: "oc_x",
      reason: "read",
    },
    async (uat) => {
      receivedUAT = uat;
      return { code: 0, data: "OK" };
    },
  );

  assert.equal(r.kind, "ok");
  assert.equal(r.result.data, "OK");
  assert.equal(receivedUAT, "uat_cached");
  // No HTTP fired — the cached UAT was usable as-is.
  assert.equal(f.calls.length, 0);
});

test("UATAccessor.invoke: refreshes proactively when access_token is near expiry", async () => {
  const { accessor, store } = buildAccessor((_url, _init, n) => {
    // First fetch is the refresh; subsequent never called.
    if (n === 1) {
      return jsonResponse({
        access_token: "uat_fresh",
        refresh_token: "ur_fresh",
        expires_in: 7200,
      });
    }
    return jsonResponse({});
  });
  // Stored UAT 30s from expiry — well inside the 60s proactive window.
  await store.set("ou_alice", {
    accessToken: "uat_stale",
    refreshToken: "ur_alive",
    expiresAt: Date.now() + 30_000,
    refreshExpiresAt: Date.now() + 604800_000,
    scope: "",
    grantedAt: Date.now() - 7170_000,
  });

  let receivedUAT;
  const r = await accessor.invoke(
    { userOpenId: "ou_alice", chatId: "oc_x", reason: "read" },
    async (uat) => {
      receivedUAT = uat;
      return { code: 0, data: "OK" };
    },
  );
  assert.equal(r.kind, "ok");
  // The handler saw the refreshed token.
  assert.equal(receivedUAT, "uat_fresh");
  const stored = await store.get("ou_alice");
  assert.equal(stored.accessToken, "uat_fresh");
});

test("UATAccessor.invoke: triggers auth flow when no UAT is stored", async () => {
  const { accessor, store, channel } = buildAccessor((_url, _init, n) => {
    // 1: device_authorization. 2: poll → token.
    if (n === 1) return jsonResponse(DEVICE_AUTH_BODY);
    return jsonResponse({
      access_token: "uat_first",
      refresh_token: "ur_first",
      expires_in: 7200,
    });
  });

  let receivedUAT;
  const r = await accessor.invoke(
    {
      userOpenId: "ou_alice",
      chatId: "oc_x",
      reason: "read your bitable",
    },
    async (uat) => {
      receivedUAT = uat;
      return { code: 0, data: "OK" };
    },
  );

  assert.equal(r.kind, "ok");
  assert.equal(receivedUAT, "uat_first");
  assert.notEqual(await store.get("ou_alice"), null);
  // Auth card was sent to the user.
  assert.equal(channel.sent.length, 1);
  assert.match(channel.sent[0].input.card.header.title.content, /Authorization/);
});

test("UATAccessor.invoke: re-runs flow + retries handler on Lark UAT-dead response", async () => {
  // The stored UAT looks fresh but Lark rejects it (revoked server-
  // side). Accessor should drop it, re-auth, retry once.
  let postReauthHandlerCalls = 0;
  const { accessor, store, channel } = buildAccessor((_url, _init, n) => {
    if (n === 1) return jsonResponse(DEVICE_AUTH_BODY);
    return jsonResponse({
      access_token: "uat_after_reauth",
      refresh_token: "ur",
      expires_in: 7200,
    });
  });

  await store.set("ou_alice", {
    accessToken: "uat_revoked",
    refreshToken: "ur",
    expiresAt: Date.now() + 7200_000,
    refreshExpiresAt: Date.now() + 604800_000,
    scope: "",
    grantedAt: Date.now(),
  });

  const seen = [];
  const r = await accessor.invoke(
    { userOpenId: "ou_alice", chatId: "oc_x", reason: "read" },
    async (uat) => {
      seen.push(uat);
      // First call: pretend Lark says UAT is dead.
      if (seen.length === 1) {
        return { code: 99991664, msg: "user_access_token expired" };
      }
      postReauthHandlerCalls++;
      return { code: 0, data: "OK" };
    },
  );

  assert.equal(r.kind, "ok");
  assert.equal(r.result.data, "OK");
  // Handler called twice: once with stale UAT, once with the fresh one.
  assert.equal(seen.length, 2);
  assert.equal(seen[0], "uat_revoked");
  assert.equal(seen[1], "uat_after_reauth");
  assert.equal(postReauthHandlerCalls, 1);
  // The original revoked UAT is gone; the fresh one replaced it.
  const stored = await store.get("ou_alice");
  assert.equal(stored.accessToken, "uat_after_reauth");
  // Auth card sent.
  assert.equal(channel.sent.length, 1);
});

test("UATAccessor.invoke: re-runs flow when handler throws SDK-shaped UAT error", async () => {
  // Lark SDK rejects with `{ code, response: { data: { code } } }`
  // shape on some calls — accessor must recognise that too.
  const { accessor, store } = buildAccessor((_url, _init, n) => {
    if (n === 1) return jsonResponse(DEVICE_AUTH_BODY);
    return jsonResponse({
      access_token: "uat_fresh",
      refresh_token: "ur",
      expires_in: 7200,
    });
  });
  await store.set("ou_alice", {
    accessToken: "uat_dead",
    refreshToken: "ur",
    expiresAt: Date.now() + 7200_000,
    refreshExpiresAt: Date.now() + 604800_000,
    scope: "",
    grantedAt: Date.now(),
  });

  let calls = 0;
  const r = await accessor.invoke(
    { userOpenId: "ou_alice", chatId: "oc_x", reason: "read" },
    async () => {
      calls++;
      if (calls === 1) {
        const err = new Error("UAT expired");
        err.response = { data: { code: 99991663 } };
        throw err;
      }
      return { code: 0, data: "OK" };
    },
  );
  assert.equal(r.kind, "ok");
  assert.equal(calls, 2);
});

test("UATAccessor.invoke: surfaces auth_failed when user denies authorization", async () => {
  const { accessor } = buildAccessor((_url, _init, n) => {
    if (n === 1) return jsonResponse(DEVICE_AUTH_BODY);
    return jsonResponse({ error: "access_denied" });
  });

  const r = await accessor.invoke(
    { userOpenId: "ou_alice", chatId: "oc_x", reason: "read" },
    async () => ({ code: 0, data: "should not reach" }),
  );
  assert.equal(r.kind, "auth_failed");
  assert.equal(r.outcome.kind, "denied");
});

test("UATAccessor.invoke: doesn't loop forever when re-auth UAT is also rejected", async () => {
  // If the fresh UAT also gets a UAT-dead response, the accessor
  // returns auth_failed instead of looping. (Likely a permission
  // / scope issue masquerading as a UAT error.)
  const { accessor, store } = buildAccessor((_url, _init, n) => {
    if (n === 1) return jsonResponse(DEVICE_AUTH_BODY);
    return jsonResponse({
      access_token: "uat_fresh_but_doomed",
      refresh_token: "ur",
      expires_in: 7200,
    });
  });
  await store.set("ou_alice", {
    accessToken: "uat_dead_1",
    refreshToken: "ur",
    expiresAt: Date.now() + 7200_000,
    refreshExpiresAt: Date.now() + 604800_000,
    scope: "",
    grantedAt: Date.now(),
  });

  let calls = 0;
  const r = await accessor.invoke(
    { userOpenId: "ou_alice", chatId: "oc_x", reason: "read" },
    async () => {
      calls++;
      const err = new Error("scope-masquerading-as-UAT-error");
      err.code = 99991663;
      throw err;
    },
  );
  assert.equal(r.kind, "auth_failed");
  assert.equal(r.outcome.kind, "error");
  // Handler called twice (once originally, once after re-auth) — not
  // infinite looping.
  assert.equal(calls, 2);
});

test("UATAccessor.invoke: non-UAT errors propagate untouched", async () => {
  const { accessor, store } = buildAccessor(() => jsonResponse({}));
  await store.set("ou_alice", {
    accessToken: "uat_x",
    refreshToken: "ur",
    expiresAt: Date.now() + 7200_000,
    refreshExpiresAt: Date.now() + 604800_000,
    scope: "",
    grantedAt: Date.now(),
  });
  await assert.rejects(
    accessor.invoke(
      { userOpenId: "ou_alice", chatId: "oc_x", reason: "read" },
      async () => {
        throw new Error("network down");
      },
    ),
    /network down/,
  );
});

test("UATAccessor.invoke: drops + reauths a UAT whose scope doesn't cover the tool's required scopes (Codex review #2)", async () => {
  // Stored UAT was minted with only `offline_access` (the regression
  // condition Codex flagged: tools never passed scopes, so OAuth grant
  // only got offline_access, then every privileged tool reauth-looped).
  // Now the tool says it needs `calendar:calendar.event:create`; the
  // accessor MUST drop the stale-scope token and run a fresh flow with
  // the wider scope, NOT silently use the insufficient one and let
  // Lark return 99991663.
  let deviceAuthScope = null;
  const { accessor, store, channel } = buildAccessor((_url, init, n) => {
    if (n === 1) {
      // Capture the scope param the device authorization request
      // sends — it must include the new wider scope, not just
      // offline_access.
      const body = new URLSearchParams(init.body);
      deviceAuthScope = body.get("scope");
      return jsonResponse(DEVICE_AUTH_BODY);
    }
    return jsonResponse({
      access_token: "uat_with_scope",
      refresh_token: "ur_x",
      expires_in: 7200,
      scope: "offline_access calendar:calendar.event:create",
    });
  });

  await store.set("ou_alice", {
    accessToken: "uat_offline_only",
    refreshToken: "ur_old",
    expiresAt: Date.now() + 7200_000,
    refreshExpiresAt: Date.now() + 604800_000,
    scope: "offline_access", // narrow scope — wouldn't pass the new check
    grantedAt: Date.now() - 60_000,
  });

  let handlerSawUat;
  const r = await accessor.invoke(
    {
      userOpenId: "ou_alice",
      chatId: "oc_x",
      reason: "create event",
      scopes: ["calendar:calendar.event:create"],
    },
    async (uat) => {
      handlerSawUat = uat;
      return { code: 0, data: "OK" };
    },
  );

  assert.equal(r.kind, "ok");
  // Handler ran with the FRESH UAT, not the stale-scope one.
  assert.equal(handlerSawUat, "uat_with_scope");
  // Auth card was sent — proves a fresh device flow ran.
  assert.equal(channel.sent.length, 1);
  // Device authorization scope param included offline_access (auto-
  // appended) AND the per-tool scope.
  assert.match(deviceAuthScope, /calendar:calendar\.event:create/);
  assert.match(deviceAuthScope, /offline_access/);
  // Stored UAT was replaced with the new one carrying the wider scope.
  const storedAfter = await store.get("ou_alice");
  assert.equal(storedAfter.accessToken, "uat_with_scope");
});

test("UATAccessor.invoke: keeps the stored UAT when scope is sufficient", async () => {
  // Counterpart to the above: a stored UAT whose scope covers
  // the request must NOT trigger reauth (would be a UX regression).
  const { accessor, store, channel, fakeFetch: f } = buildAccessor(() =>
    jsonResponse({}),
  );

  await store.set("ou_alice", {
    accessToken: "uat_wide",
    refreshToken: "ur_x",
    expiresAt: Date.now() + 7200_000,
    refreshExpiresAt: Date.now() + 604800_000,
    scope: "offline_access calendar:calendar.event:read calendar:calendar.event:create",
    grantedAt: Date.now() - 60_000,
  });

  let handlerSawUat;
  const r = await accessor.invoke(
    {
      userOpenId: "ou_alice",
      chatId: "oc_x",
      reason: "create event",
      scopes: ["calendar:calendar.event:create"],
    },
    async (uat) => {
      handlerSawUat = uat;
      return { code: 0, data: "OK" };
    },
  );

  assert.equal(r.kind, "ok");
  assert.equal(handlerSawUat, "uat_wide");
  // No fresh auth flow.
  assert.equal(channel.sent.length, 0);
  // No HTTP at all (no refresh either, UAT not near expiry).
  assert.equal(f.calls.length, 0);
});

test("UAT_DEAD_CODES carries the documented Lark codes", () => {
  // Sanity: a doc / code drift here would silently break auto-retry.
  assert.equal(UAT_DEAD_CODES.has(99991663), true);
  assert.equal(UAT_DEAD_CODES.has(99991664), true);
  assert.equal(UAT_DEAD_CODES.has(99991668), true);
  assert.equal(UAT_DEAD_CODES.has(99991669), true);
  assert.equal(UAT_DEAD_CODES.has(0), false);
});
