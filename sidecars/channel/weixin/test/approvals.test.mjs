import test from "node:test";
import assert from "node:assert/strict";

import { matchDecision, WeixinApprovals } from "../dist/approvals.js";

test("matchDecision: English yes/no/always forms", () => {
  assert.equal(matchDecision("yes"), "approve");
  assert.equal(matchDecision("Y"), "approve");
  assert.equal(matchDecision("ok"), "approve");
  assert.equal(matchDecision("always"), "approve_always");
  assert.equal(matchDecision("approve_always"), "approve_always");
  assert.equal(matchDecision("no"), "deny");
  assert.equal(matchDecision("N"), "deny");
  assert.equal(matchDecision("deny"), "deny");
  assert.equal(matchDecision("reject"), "deny");
});

test("matchDecision: only accepts English keywords", () => {
  // Chinese / other-locale forms intentionally fall through to the
  // agent as normal chat, so approval vocabulary stays unambiguous.
  assert.equal(matchDecision("批准"), null);
  assert.equal(matchDecision("始终"), null);
  assert.equal(matchDecision("拒绝"), null);
});

test("matchDecision: trims and normalises case", () => {
  assert.equal(matchDecision("  YES  "), "approve");
  assert.equal(matchDecision("\nNo\t"), "deny");
});

test("matchDecision: returns null for non-keyword text", () => {
  assert.equal(matchDecision("hello"), null);
  assert.equal(matchDecision(""), null);
  assert.equal(matchDecision("maybe"), null);
  assert.equal(matchDecision("yes please"), null);
});

function stubLogger() {
  return { debug() {}, info() {}, warn() {}, error() {} };
}

function fakeHandle() {
  return {
    accountId: "bot1",
    state: {
      accountId: "bot1",
      botToken: "tok",
      baseUrl: "https://example",
      userId: "u",
      getUpdatesBuf: "",
      configMgr: null,
      mdFilter: null,
      abort: new AbortController(),
      contextTokens: new Map(),
    },
  };
}

test("tryResolveInbound returns false when no pending", () => {
  const a = new WeixinApprovals(stubLogger());
  const handled = a.tryResolveInbound("bot1", { toUserId: "u@im.wechat" }, "yes");
  assert.equal(handled, false);
});

test("onRequested + tryResolveInbound happy path resolves with correct decision", async () => {
  const a = new WeixinApprovals(stubLogger());
  const handle = fakeHandle();
  const chat = { toUserId: "u@im.wechat" };
  // Don't actually call sendMessage — intercept fetch by pointing
  // baseUrl at something we'll let fail silently. The pending promise
  // still resolves via tryResolveInbound regardless of prompt send.
  const pending = a.onRequested(
    {
      callId: "c1",
      sessionId: "s",
      userId: "u",
      tool: "bash",
      paramsPreview: "ls -la",
    },
    { botId: "bot1", handle, chat },
  );
  // Give the fire-and-forget prompt a tick to run (and fail in this
  // environment since there's no HTTP server). We don't await it.
  await new Promise((r) => setImmediate(r));
  const handled = a.tryResolveInbound("bot1", chat, "yes");
  assert.equal(handled, true);
  assert.equal(await pending, "approve");
});

test("tryResolveInbound ignores non-keyword text and leaves pending intact", async () => {
  const a = new WeixinApprovals(stubLogger());
  const handle = fakeHandle();
  const chat = { toUserId: "u@im.wechat" };
  const pending = a.onRequested(
    {
      callId: "c2",
      sessionId: "s",
      userId: "u",
      tool: "bash",
      paramsPreview: "",
    },
    { botId: "bot1", handle, chat },
  );
  await new Promise((r) => setImmediate(r));
  assert.equal(a.tryResolveInbound("bot1", chat, "hmm"), false);
  // Can still resolve afterwards
  assert.equal(a.tryResolveInbound("bot1", chat, "no"), true);
  assert.equal(await pending, "deny");
});

test("onRequested for a route=null auto-denies without touching state", async () => {
  const a = new WeixinApprovals(stubLogger());
  const decision = await a.onRequested(
    {
      callId: "c3",
      sessionId: "s",
      userId: "u",
      tool: "bash",
      paramsPreview: "",
    },
    null,
  );
  assert.equal(decision, "deny");
});

test("onBotStopped resolves all pending for that bot as deny", async () => {
  const a = new WeixinApprovals(stubLogger());
  const h = fakeHandle();
  const chat = { toUserId: "u@im.wechat" };
  const p = a.onRequested(
    { callId: "c4", sessionId: "s", userId: "u", tool: "bash", paramsPreview: "" },
    { botId: "bot1", handle: h, chat },
  );
  await new Promise((r) => setImmediate(r));
  a.onBotStopped("bot1");
  assert.equal(await p, "deny");
});

test("onStop flushes every pending", async () => {
  const a = new WeixinApprovals(stubLogger());
  const h = fakeHandle();
  const p1 = a.onRequested(
    { callId: "x1", sessionId: "s", userId: "u", tool: "t", paramsPreview: "" },
    { botId: "bot1", handle: h, chat: { toUserId: "u1@im.wechat" } },
  );
  const p2 = a.onRequested(
    { callId: "x2", sessionId: "s", userId: "u", tool: "t", paramsPreview: "" },
    { botId: "bot2", handle: h, chat: { toUserId: "u2@im.wechat" } },
  );
  await new Promise((r) => setImmediate(r));
  a.onStop();
  assert.equal(await p1, "deny");
  assert.equal(await p2, "deny");
});

test("second onRequested for same (bot, user) supersedes the first (old denies)", async () => {
  const a = new WeixinApprovals(stubLogger());
  const h = fakeHandle();
  const chat = { toUserId: "u@im.wechat" };
  const first = a.onRequested(
    { callId: "a", sessionId: "s", userId: "u", tool: "t", paramsPreview: "" },
    { botId: "bot1", handle: h, chat },
  );
  const second = a.onRequested(
    { callId: "b", sessionId: "s", userId: "u", tool: "t", paramsPreview: "" },
    { botId: "bot1", handle: h, chat },
  );
  await new Promise((r) => setImmediate(r));
  assert.equal(await first, "deny");
  // second is still pending; resolve it explicitly
  assert.equal(a.tryResolveInbound("bot1", chat, "yes"), true);
  assert.equal(await second, "approve");
});
