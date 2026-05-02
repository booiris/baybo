import test from "node:test";
import assert from "node:assert/strict";

import { AuthFlowController } from "../dist/auth/auth-flow.js";
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
  let nextMessageId = 1;
  return {
    sent,
    updated,
    async send(chatId, input) {
      const messageId = `om_${nextMessageId++}`;
      sent.push({ chatId, input, messageId });
      return { messageId };
    },
    async updateCard(messageId, card) {
      updated.push({ messageId, card });
    },
  };
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

const DEVICE_AUTH_BODY = {
  device_code: "DC_xxx",
  user_code: "ABCD-1234",
  verification_uri: "https://accounts.feishu.cn/oauth/v1/device",
  verification_uri_complete:
    "https://accounts.feishu.cn/oauth/v1/device?user_code=ABCD-1234",
  expires_in: 240,
  interval: 0, // poll immediately for tests
};

function buildController(handler) {
  const scope = new MemoryScope();
  const store = new UATStore(scope);
  const channel = fakeChannel();
  const f = fakeFetch(handler);
  const controller = new AuthFlowController({
    channel,
    store,
    appId: "cli_x",
    appSecret: "sec_y",
    baseUrl: "https://open.feishu.cn",
    logger: noopLogger,
    fetchImpl: f.fetch,
  });
  return { controller, store, channel, fakeFetch: f };
}

test("AuthFlowController: happy path stores UAT and edits card to authorized", async () => {
  // First call: device_authorization. Second call onwards: poll → token.
  const { controller, store, channel, fakeFetch: f } = buildController(
    (_url, _init, n) => {
      if (n === 1) return jsonResponse(DEVICE_AUTH_BODY);
      return jsonResponse({
        access_token: "uat_v2_xxx",
        refresh_token: "ur_v2_yyy",
        expires_in: 7200,
        refresh_token_expires_in: 604800,
        scope: "im:message offline_access",
      });
    },
  );

  const r = await controller.requestUAT({
    userOpenId: "ou_alice",
    chatId: "oc_test",
    reason: "read your Feishu calendar",
  });
  assert.equal(r.kind, "ok");
  assert.equal(r.uat.accessToken, "uat_v2_xxx");

  // UAT landed in the store.
  const stored = await store.get("ou_alice");
  assert.equal(stored.accessToken, "uat_v2_xxx");

  // One pending card sent, then mutated to "Authorized".
  assert.equal(channel.sent.length, 1);
  assert.equal(channel.sent[0].chatId, "oc_test");
  assert.equal(channel.updated.length, 1);
  assert.equal(channel.updated[0].messageId, channel.sent[0].messageId);
  assert.equal(channel.updated[0].card.header.template, "green");
  assert.match(channel.updated[0].card.header.title.content, /Authorized/);

  // Sanity: device_authorization called once + at least one poll.
  assert.ok(f.calls.length >= 2);
});

test("AuthFlowController: pending card carries verification URI + user code", async () => {
  const { controller, channel } = buildController((_u, _i, n) => {
    if (n === 1) return jsonResponse(DEVICE_AUTH_BODY);
    // Force a slow poll so we can inspect the pending card before
    // it transitions; abort after to clean up.
    return jsonResponse({ error: "authorization_pending" });
  });

  // Fire-and-cancel: kick off a flow then immediately cancel.
  // The send happens before the poll loop starts, so by the time we
  // cancel the pending card has already landed.
  const promise = controller.requestUAT({
    userOpenId: "ou_alice",
    chatId: "oc_test",
    reason: "search your wiki",
  });
  // Yield once so requestDeviceAuthorization + send can complete.
  await new Promise((r) => setImmediate(r));
  controller.cancelAll();
  const r = await promise;
  assert.equal(r.kind, "cancelled");

  // Pending card was sent with the correct content.
  assert.equal(channel.sent.length, 1);
  const card = channel.sent[0].input.card;
  assert.equal(card.header.template, "orange");
  // Auth URL button present and correct.
  const action = card.elements.find((e) => e.tag === "action");
  assert.equal(
    action.actions[0].url,
    "https://accounts.feishu.cn/oauth/v1/device?user_code=ABCD-1234",
  );
  // User code rendered in the body.
  const body = card.elements.find((e) => e.tag === "div").text.content;
  assert.match(body, /ABCD-1234/);
  assert.match(body, /search your wiki/);
});

test("AuthFlowController: access_denied edits card to denied + returns denied", async () => {
  const { controller, channel } = buildController((_u, _i, n) => {
    if (n === 1) return jsonResponse(DEVICE_AUTH_BODY);
    return jsonResponse({ error: "access_denied" });
  });

  const r = await controller.requestUAT({
    userOpenId: "ou_alice",
    chatId: "oc_test",
    reason: "read",
  });
  assert.equal(r.kind, "denied");
  // Yield once so the best-effort terminal card update lands.
  await new Promise((r) => setImmediate(r));
  assert.equal(channel.updated.length, 1);
  assert.equal(channel.updated[0].card.header.template, "red");
  assert.match(channel.updated[0].card.header.title.content, /denied/);
});

test("AuthFlowController: device_authorization failure surfaces error without sending a card", async () => {
  const { controller, channel } = buildController(() =>
    jsonResponse(
      { error: "invalid_client", error_description: "app_id mismatch" },
      false,
    ),
  );

  const r = await controller.requestUAT({
    userOpenId: "ou_alice",
    chatId: "oc_test",
    reason: "read",
  });
  assert.equal(r.kind, "error");
  assert.match(r.message, /app_id mismatch/);
  // No card sent — we never got past device_authorization.
  assert.equal(channel.sent.length, 0);
});

test("AuthFlowController: simultaneous calls for the same user join one flow", async () => {
  // Hold the poll open; when we finally release, both joiners get
  // the same token. This proves the dedup map joins, doesn't restart.
  let releasePoll;
  const pollGate = new Promise((r) => (releasePoll = r));
  const { controller, channel, fakeFetch: f } = buildController(
    async (_u, _i, n) => {
      if (n === 1) return jsonResponse(DEVICE_AUTH_BODY);
      // The first poll arrives quickly; gate it.
      await pollGate;
      return jsonResponse({
        access_token: "uat_xx",
        refresh_token: "ur_yy",
        expires_in: 7200,
      });
    },
  );

  const a = controller.requestUAT({
    userOpenId: "ou_alice",
    chatId: "oc_test",
    reason: "first",
  });
  const b = controller.requestUAT({
    userOpenId: "ou_alice",
    chatId: "oc_test",
    reason: "second",
  });

  // Both joiners share the same Promise — proven by the outcome
  // being the same object after release.
  releasePoll();
  const [resultA, resultB] = await Promise.all([a, b]);
  assert.equal(resultA.kind, "ok");
  assert.equal(resultB.kind, "ok");
  // Same UAT in both — would fail if we'd run two separate flows.
  assert.equal(resultA.uat.accessToken, resultB.uat.accessToken);

  // One card sent, not two — dedup actually dedupped.
  assert.equal(channel.sent.length, 1);
  // device_authorization called once.
  const deviceAuthCalls = f.calls.filter((c) =>
    c.url.endsWith("/device_authorization"),
  );
  assert.equal(deviceAuthCalls.length, 1);
});

test("AuthFlowController: completed flow clears inflight map so the next call starts fresh", async () => {
  // Without map cleanup, a finished flow's promise would shadow new
  // requests and they'd all resolve to the stale outcome.
  let pollResponses = 0;
  const { controller } = buildController((_u, _i, n) => {
    if (n === 1) return jsonResponse(DEVICE_AUTH_BODY);
    if (n === 2) {
      pollResponses++;
      return jsonResponse({
        access_token: "uat_first",
        refresh_token: "ur_first",
        expires_in: 7200,
      });
    }
    if (n === 3) return jsonResponse(DEVICE_AUTH_BODY);
    pollResponses++;
    return jsonResponse({
      access_token: "uat_second",
      refresh_token: "ur_second",
      expires_in: 7200,
    });
  });

  const first = await controller.requestUAT({
    userOpenId: "ou_alice",
    chatId: "oc_test",
    reason: "first",
  });
  // Inflight cleared.
  assert.equal(controller.isInflight("ou_alice"), false);

  const second = await controller.requestUAT({
    userOpenId: "ou_alice",
    chatId: "oc_test",
    reason: "second",
  });
  assert.equal(first.uat.accessToken, "uat_first");
  assert.equal(second.uat.accessToken, "uat_second");
  assert.equal(pollResponses, 2);
});

test("AuthFlowController: send-card failure surfaces as error and doesn't try to update", async () => {
  const channelFailing = {
    async send() {
      throw new Error("Lark rejected card: invalid_chat_id");
    },
    async updateCard() {
      throw new Error("would never be called");
    },
  };
  const f = fakeFetch(() => jsonResponse(DEVICE_AUTH_BODY));
  const controller = new AuthFlowController({
    channel: channelFailing,
    store: new UATStore(new MemoryScope()),
    appId: "x",
    appSecret: "y",
    baseUrl: "https://open.feishu.cn",
    logger: noopLogger,
    fetchImpl: f.fetch,
  });

  const r = await controller.requestUAT({
    userOpenId: "ou_alice",
    chatId: "oc_invalid",
    reason: "read",
  });
  assert.equal(r.kind, "error");
  assert.match(r.message, /failed to send auth card/);
  assert.match(r.message, /invalid_chat_id/);
});
