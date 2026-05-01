/**
 * Regression coverage for BotChannel's StartBot/StopBot + polling-exit
 * lifecycle, using a stub platform so we don't have to stand up
 * grammy or a real WS fixture.
 */
import test from "node:test";
import assert from "node:assert/strict";

import { BotChannel } from "../dist/bot.js";

function stubLogger() {
  return { debug() {}, info() {}, warn() {}, error() {} };
}

/**
 * `fake.emit(ev)` and `fake.resolveExit(err?)` drive whichever bot
 * generation was spawned most recently. `fake.forHandle(id)` returns
 * the per-generation `{ hooks, handle, resolveExit, rejectExit }` so
 * race tests can fire a specific prior generation's exit independent
 * of the current one.
 */
function makePlatform() {
  const calls = { starts: [], stops: [], sends: [] };
  const byHandleId = new Map();
  let nextHandleId = 0;
  let latest = null;
  return {
    calls,
    emit: (ev) => latest.hooks.emit(ev),
    resolveExit: () => latest.resolveExit(),
    rejectExit: (err) => latest.rejectExit(err),
    forHandle: (id) => byHandleId.get(id),
    platform: {
      async startBot(cmd, hooks) {
        const id = nextHandleId++;
        const handle = { id, botId: cmd.botId, stopped: false };
        await hooks.attach(handle);
        calls.starts.push({ botId: cmd.botId, handleId: id });
        let resolveExit;
        let rejectExit;
        const waitForExit = new Promise((resolve, reject) => {
          resolveExit = resolve;
          rejectExit = reject;
        });
        const entry = { hooks, handle, resolveExit, rejectExit };
        byHandleId.set(id, entry);
        latest = entry;
        return { handle, username: `bot${id}`, waitForExit };
      },
      async stopBot(handle) {
        handle.stopped = true;
        calls.stops.push(handle.id);
      },
      async sendText(handle, chat, text) {
        if (handle.stopped) throw new Error("handle stopped");
        calls.sends.push({ handleId: handle.id, chat, text });
      },
    },
  };
}

test("explicit StopBot notifies approvals before tearing down the handle", async () => {
  const { platform } = makePlatform();
  const events = [];
  const approvals = {
    async onRequested() { return "approve"; },
    onBotStopped(botId) { events.push(`botStopped:${botId}`); },
  };
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform,
    approvals,
  });
  await channel.onStartBot({ botId: "b1", token: "t" });
  const report = await channel.onStopBot({ botId: "b1" });
  assert.equal(report.ok, true);
  assert.deepEqual(events, ["botStopped:b1"]);
});

test("polling exit keeps user routes so a reconnect delivers existing users", async () => {
  const fake = makePlatform();
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: fake.platform,
  });

  await channel.onStartBot({ botId: "b1", token: "t1" });
  fake.emit({ chat: 42, platformUserId: "u1", content: "hi" });

  fake.rejectExit(new Error("boom"));
  // let the waitForExit .catch chain run
  await new Promise((r) => setImmediate(r));

  await channel.onStartBot({ botId: "b1", token: "t2" });

  await channel.onMessage({
    sessionId: "s",
    userId: "test_b1_42_u1",
    content: "hello again",
  });
  assert.equal(fake.calls.sends.length, 1);
  assert.equal(fake.calls.sends[0].text, "hello again");
  assert.equal(fake.calls.sends[0].handleId, 1);
});

test("polling exit still notifies approvals so pending calls don't leak", async () => {
  const fake = makePlatform();
  const events = [];
  const approvals = {
    async onRequested() { return "approve"; },
    onBotStopped(botId) { events.push(`botStopped:${botId}`); },
  };
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: fake.platform,
    approvals,
  });
  await channel.onStartBot({ botId: "b1", token: "t" });
  fake.rejectExit(new Error("boom"));
  await new Promise((r) => setImmediate(r));
  assert.deepEqual(events, ["botStopped:b1"]);
});

function captureLogger() {
  const log = { debug: [], info: [], warn: [], error: [] };
  return {
    log,
    logger: {
      debug: (...a) => log.debug.push(a),
      info: (...a) => log.info.push(a),
      warn: (...a) => log.warn.push(a),
      error: (...a) => log.error.push(a),
    },
  };
}

test("slashCommands configured + platform supports it: registers once with the manifest", async () => {
  const fake = makePlatform();
  const calls = [];
  fake.platform.registerSlashCommands = async (handle, commands) => {
    calls.push({ handleId: handle.id, commands: [...commands] });
  };
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: fake.platform,
    slashCommands: [{ command: "new", description: "fresh session" }],
  });
  await channel.onStartBot({ botId: "b1", token: "t" });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].handleId, 0);
  assert.deepEqual(calls[0].commands, [
    { command: "new", description: "fresh session" },
  ]);
});

test("slashCommands configured but platform does not implement registerSlashCommands: warns once", async () => {
  const fake = makePlatform();
  // platform intentionally has no registerSlashCommands method
  const { log, logger } = captureLogger();
  const channel = new BotChannel({
    channelType: "test",
    logger,
    platform: fake.platform,
    slashCommands: [{ command: "new", description: "fresh session" }],
  });
  await channel.onStartBot({ botId: "b1", token: "t" });
  await channel.onStartBot({ botId: "b2", token: "t" });
  // Single warn even across multiple StartBot calls.
  assert.equal(log.warn.length, 1);
  assert.match(String(log.warn[0][0]), /registerSlashCommands/);
});

test("slashCommands unset: registerSlashCommands is never called", async () => {
  const fake = makePlatform();
  let called = 0;
  fake.platform.registerSlashCommands = async () => {
    called++;
  };
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: fake.platform,
  });
  await channel.onStartBot({ botId: "b1", token: "t" });
  assert.equal(called, 0);
});

test("onSlashManifest replaces seed list and re-publishes to every live bot", async () => {
  const fake = makePlatform();
  const calls = [];
  fake.platform.registerSlashCommands = async (handle, commands) => {
    calls.push({ handleId: handle.id, commands: [...commands] });
  };
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: fake.platform,
    slashCommands: [{ command: "old", description: "legacy" }],
  });
  await channel.onStartBot({ botId: "b1", token: "t1" });
  await channel.onStartBot({ botId: "b2", token: "t2" });
  // Seed list was published once per StartBot.
  assert.equal(calls.length, 2);
  assert.deepEqual(calls[0].commands, [{ command: "old", description: "legacy" }]);

  await channel.onSlashManifest([
    { command: "new", description: "fresh" },
  ]);
  // Each live bot got re-registered with the new manifest.
  assert.equal(calls.length, 4);
  const re = calls.slice(2);
  assert.deepEqual(
    re.map((c) => c.handleId).sort(),
    [0, 1],
  );
  for (const c of re) {
    assert.deepEqual(c.commands, [{ command: "new", description: "fresh" }]);
  }
});

test("onSlashManifest before StartBot is honoured at bot startup", async () => {
  const fake = makePlatform();
  const calls = [];
  fake.platform.registerSlashCommands = async (handle, commands) => {
    calls.push({ handleId: handle.id, commands: [...commands] });
  };
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: fake.platform,
  });
  await channel.onSlashManifest([
    { command: "new", description: "fresh" },
  ]);
  // No bots yet -> no publishes.
  assert.equal(calls.length, 0);

  await channel.onStartBot({ botId: "b1", token: "t" });
  assert.equal(calls.length, 1);
  assert.deepEqual(calls[0].commands, [
    { command: "new", description: "fresh" },
  ]);
});

test("onSlashManifest with an empty list publishes nothing", async () => {
  const fake = makePlatform();
  let called = 0;
  fake.platform.registerSlashCommands = async () => {
    called++;
  };
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: fake.platform,
    slashCommands: [{ command: "old", description: "legacy" }],
  });
  await channel.onStartBot({ botId: "b1", token: "t" });
  assert.equal(called, 1);

  await channel.onSlashManifest([]);
  // The manifest was cleared; no further calls.
  assert.equal(called, 1);
  // And future StartBot also publishes nothing.
  await channel.onStartBot({ botId: "b2", token: "t" });
  assert.equal(called, 1);
});

test("registerSlashCommands throwing does not fail bot startup", async () => {
  const fake = makePlatform();
  fake.platform.registerSlashCommands = async () => {
    throw new Error("api blew up");
  };
  const { log, logger } = captureLogger();
  const channel = new BotChannel({
    channelType: "test",
    logger,
    platform: fake.platform,
    slashCommands: [{ command: "new", description: "fresh session" }],
  });
  const report = await channel.onStartBot({ botId: "b1", token: "t" });
  assert.equal(report.ok, true);
  assert.equal(log.warn.length, 1);
  assert.match(String(log.warn[0][0]), /registerSlashCommands failed/);
});

test("a stale polling-exit from a prior generation cannot delete the fresh handle", async () => {
  const fake = makePlatform();
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: fake.platform,
  });
  await channel.onStartBot({ botId: "b1", token: "t1" });

  await channel.onStopBot({ botId: "b1" });
  await channel.onStartBot({ botId: "b1", token: "t2" });

  // Grammy's bot.start() promise resolves on a microtask after
  // bot.stop() returns — simulate the OLD handle's waitForExit
  // resolving AFTER a fresh bot already took the slot.
  fake.forHandle(0).resolveExit();
  await new Promise((r) => setImmediate(r));

  // The new handle (id=1) must still be live.
  fake.emit({ chat: 99, platformUserId: "u2", content: "new" });
  await channel.onMessage({
    sessionId: "s",
    userId: "test_b1_99_u2",
    content: "reply",
  });
  assert.equal(fake.calls.sends.length, 1);
  assert.equal(fake.calls.sends[0].handleId, 1);
});

test("stale exit rejected even when the platform returns a stable handle value across generations", async () => {
  // Repros the reviewer's concern: a transport that identifies bots by
  // a stable value (e.g. returns `cmd.botId` as the handle) breaks any
  // reference-based stale-exit guard. The channel must use its own
  // generation token to stay correct.
  const calls = { sends: [] };
  const exits = [];
  let lastEmit = null;
  const platform = {
    async startBot(cmd, hooks) {
      lastEmit = hooks.emit;
      await hooks.attach(cmd.botId);
      let resolveExit;
      const waitForExit = new Promise((res) => { resolveExit = res; });
      exits.push(resolveExit);
      // Same string value for every generation of the same botId.
      return { handle: cmd.botId, waitForExit };
    },
    async stopBot() {},
    async sendText(handle, chat, text) {
      calls.sends.push({ handle, chat, text });
    },
  };
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform,
  });
  await channel.onStartBot({ botId: "b1", token: "t1" });
  await channel.onStopBot({ botId: "b1" });
  await channel.onStartBot({ botId: "b1", token: "t2" });

  // Gen 0's exit resolves late. A reference check would falsely
  // match the fresh slot (handle value identical). The internal
  // generation counter must reject it.
  exits[0]();
  await new Promise((r) => setImmediate(r));

  // Fresh generation still live — ingest + outbound confirms.
  lastEmit({ chat: 1, platformUserId: "u", content: "hi" });
  await channel.onMessage({
    sessionId: "s",
    userId: "test_b1_1_u",
    content: "reply",
  });
  assert.equal(calls.sends.length, 1);
  assert.equal(calls.sends[0].text, "reply");
});

test("structured ChatId keeps same-user cross-topic sessions separate and routes outbound back", async () => {
  // ChatId is `{ chatId, threadId? }` — Telegram supergroup-topic
  // analog. The same (bot, chat, user) with different threadIds must
  // compose to distinct aura userIds so sessions stay isolated, and
  // each outbound must land back on the structured address it came
  // from. Uses the SDK default `chatKey` (no platform override) to
  // prove the default handles composite addresses correctly.
  const calls = { sends: [] };
  let lastEmit = null;
  const platform = {
    async startBot(cmd, hooks) {
      lastEmit = hooks.emit;
      await hooks.attach(cmd.botId);
      return { handle: cmd.botId, waitForExit: new Promise(() => {}) };
    },
    async stopBot() {},
    async sendText(handle, chat, text) {
      calls.sends.push({ chat, text });
    },
  };
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform,
  });
  await channel.onStartBot({ botId: "b1", token: "t" });

  lastEmit({
    chat: { chatId: 42, threadId: 1 },
    platformUserId: "u",
    content: "t1",
  });
  lastEmit({
    chat: { chatId: 42, threadId: 2 },
    platformUserId: "u",
    content: "t2",
  });
  lastEmit({ chat: { chatId: 42 }, platformUserId: "u", content: "main" });

  // Default chatKey: sorted `key=value&…`.
  await channel.onMessage({
    sessionId: "s",
    userId: "test_b1_chatId=42&threadId=1_u",
    content: "reply to topic 1",
  });
  await channel.onMessage({
    sessionId: "s",
    userId: "test_b1_chatId=42&threadId=2_u",
    content: "reply to topic 2",
  });
  await channel.onMessage({
    sessionId: "s",
    userId: "test_b1_chatId=42_u",
    content: "reply to main",
  });

  assert.equal(calls.sends.length, 3);
  assert.deepEqual(calls.sends[0].chat, { chatId: 42, threadId: 1 });
  assert.deepEqual(calls.sends[1].chat, { chatId: 42, threadId: 2 });
  assert.deepEqual(calls.sends[2].chat, { chatId: 42 });
});

test("inbound delivers events across a reconnect (new iterator after abort)", async () => {
  const fake = makePlatform();
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: fake.platform,
  });
  await channel.onStartBot({ botId: "b1", token: "t" });

  const abort1 = new AbortController();
  const iter1 = channel.inbound(abort1.signal)[Symbol.asyncIterator]();
  fake.emit({ chat: 42, platformUserId: "u1", content: "first" });
  const first = await iter1.next();
  assert.equal(first.value.content, "first");

  abort1.abort();
  const ended = await iter1.next();
  assert.equal(ended.done, true);

  // A message during the disconnect window must buffer, not drop.
  fake.emit({ chat: 42, platformUserId: "u1", content: "buffered" });

  const abort2 = new AbortController();
  const iter2 = channel.inbound(abort2.signal)[Symbol.asyncIterator]();
  const buffered = await iter2.next();
  assert.equal(buffered.done, false);
  assert.equal(buffered.value.content, "buffered");

  fake.emit({ chat: 42, platformUserId: "u1", content: "live" });
  const live = await iter2.next();
  assert.equal(live.value.content, "live");
});

test("StopBot after a polling crash still purges stale user routes", async () => {
  const fake = makePlatform();
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: fake.platform,
  });
  await channel.onStartBot({ botId: "b1", token: "t" });
  fake.emit({ chat: 42, platformUserId: "u1", content: "hi" });

  fake.rejectExit(new Error("boom"));
  await new Promise((r) => setImmediate(r));

  await channel.onStopBot({ botId: "b1" });

  await channel.onStartBot({ botId: "b1", token: "t2" });
  await channel.onMessage({
    sessionId: "s",
    userId: "test_b1_42_u1",
    content: "should drop",
  });
  assert.equal(fake.calls.sends.length, 0);
});

test("ingest fires platform.notifyTyping once per inbound with the routed handle", async () => {
  const fake = makePlatform();
  const typed = [];
  fake.platform.notifyTyping = async (handle, chat) => {
    typed.push({ handleId: handle.id, chat });
  };
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: fake.platform,
  });
  await channel.onStartBot({ botId: "b1", token: "t" });
  fake.emit({ chat: { chatId: 42, threadId: 7 }, platformUserId: "u", content: "hi" });
  fake.emit({ chat: { chatId: 42 }, platformUserId: "u", content: "again" });
  // fire-and-forget — let the microtask run
  await new Promise((r) => setImmediate(r));
  assert.deepEqual(typed, [
    { handleId: 0, chat: { chatId: 42, threadId: 7 } },
    { handleId: 0, chat: { chatId: 42 } },
  ]);
});

test("typing session keeps pinging at the refresh cadence until onMessage terminates it", async () => {
  const fake = makePlatform();
  const typed = [];
  fake.platform.notifyTyping = async () => { typed.push(Date.now()); };
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: fake.platform,
    typingRefreshMs: 15,
    typingSafetyMs: 5000,
  });
  await channel.onStartBot({ botId: "b1", token: "t" });
  fake.emit({ chat: 42, platformUserId: "u", content: "hi" });

  await new Promise((r) => setTimeout(r, 70));
  assert.ok(typed.length >= 3, `expected ≥3 pings, got ${typed.length}`);

  const before = typed.length;
  await channel.onMessage({ sessionId: "s", userId: "test_b1_42_u", content: "reply" });
  await new Promise((r) => setTimeout(r, 60));
  assert.equal(typed.length, before, "no pings after onMessage stop");
});

test("double-send keeps typing alive between reply A and reply B (pending-turn counter)", async () => {
  const fake = makePlatform();
  let typed = 0;
  fake.platform.notifyTyping = async () => { typed++; };
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: fake.platform,
    typingRefreshMs: 15,
    typingSafetyMs: 5000,
  });
  await channel.onStartBot({ botId: "b1", token: "t" });

  fake.emit({ chat: 42, platformUserId: "u", content: "A" });
  fake.emit({ chat: 42, platformUserId: "u", content: "B" });
  await new Promise((r) => setTimeout(r, 50));
  const beforeReplyA = typed;
  assert.ok(beforeReplyA >= 3, `expected ≥3 pings before any reply, got ${beforeReplyA}`);

  // Reply to A — B still pending, typing must continue.
  await channel.onMessage({ sessionId: "s", userId: "test_b1_42_u", content: "reply A" });
  await new Promise((r) => setTimeout(r, 40));
  assert.ok(
    typed > beforeReplyA,
    `ticker must keep firing while B is pending (before=${beforeReplyA}, now=${typed})`,
  );

  // Reply to B — pending reaches 0, typing stops.
  const beforeReplyB = typed;
  await channel.onMessage({ sessionId: "s", userId: "test_b1_42_u", content: "reply B" });
  await new Promise((r) => setTimeout(r, 60));
  assert.equal(typed, beforeReplyB, "no pings after the final reply");
});

test("safety cap is not extended by subsequent inbounds from the same user", async () => {
  const fake = makePlatform();
  let typed = 0;
  fake.platform.notifyTyping = async () => { typed++; };
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: fake.platform,
    typingRefreshMs: 10,
    typingSafetyMs: 60,
  });
  await channel.onStartBot({ botId: "b1", token: "t" });

  // Wedged agent: user keeps typing, bot never replies. Repeat
  // inbounds must NOT slide the safety deadline set at t=0.
  fake.emit({ chat: 42, platformUserId: "u", content: "1" });
  await new Promise((r) => setTimeout(r, 25));
  fake.emit({ chat: 42, platformUserId: "u", content: "2" });
  await new Promise((r) => setTimeout(r, 25));
  fake.emit({ chat: 42, platformUserId: "u", content: "3" });
  // Past the original 60ms cap from the first inbound.
  await new Promise((r) => setTimeout(r, 40));
  const atCap = typed;
  await new Promise((r) => setTimeout(r, 60));
  assert.equal(typed, atCap, `inbounds must not extend the safety cap (atCap=${atCap})`);
});

test("typing session stops on onApprovalRequested (bot is waiting on the user, not processing)", async () => {
  const fake = makePlatform();
  const typed = [];
  fake.platform.notifyTyping = async () => { typed.push(1); };
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: fake.platform,
    approvals: { async onRequested() { return "approve"; } },
    typingRefreshMs: 15,
    typingSafetyMs: 5000,
  });
  await channel.onStartBot({ botId: "b1", token: "t" });
  fake.emit({ chat: 42, platformUserId: "u", content: "hi" });
  await new Promise((r) => setTimeout(r, 50));
  const before = typed.length;
  assert.ok(before >= 2);

  await channel.onApprovalRequested({
    callId: "c1",
    sessionId: "s",
    userId: "test_b1_42_u",
    tool: "tool",
    paramsPreview: "",
  });
  await new Promise((r) => setTimeout(r, 60));
  assert.equal(typed.length, before, "no pings after approval request");
});

test("typing safety timeout caps an orphan session when no outbound ever arrives", async () => {
  const fake = makePlatform();
  let typed = 0;
  fake.platform.notifyTyping = async () => { typed++; };
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: fake.platform,
    typingRefreshMs: 10,
    typingSafetyMs: 40,
  });
  await channel.onStartBot({ botId: "b1", token: "t" });
  fake.emit({ chat: 42, platformUserId: "u", content: "hi" });

  await new Promise((r) => setTimeout(r, 120));
  const atCap = typed;
  await new Promise((r) => setTimeout(r, 60));
  assert.equal(typed, atCap, `no pings after safety cap (atCap=${atCap})`);
});

test("StopBot cancels pending typing sessions for that bot", async () => {
  const fake = makePlatform();
  let typed = 0;
  fake.platform.notifyTyping = async () => { typed++; };
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: fake.platform,
    typingRefreshMs: 10,
    typingSafetyMs: 5000,
  });
  await channel.onStartBot({ botId: "b1", token: "t" });
  fake.emit({ chat: 42, platformUserId: "u", content: "hi" });
  await new Promise((r) => setTimeout(r, 40));
  const before = typed;
  assert.ok(before >= 2);

  await channel.onStopBot({ botId: "b1" });
  await new Promise((r) => setTimeout(r, 60));
  assert.equal(typed, before, "no pings after StopBot");
});

test("polling exit cancels typing sessions (but preserves user routes)", async () => {
  const fake = makePlatform();
  let typed = 0;
  fake.platform.notifyTyping = async () => { typed++; };
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: fake.platform,
    typingRefreshMs: 10,
    typingSafetyMs: 5000,
  });
  await channel.onStartBot({ botId: "b1", token: "t" });
  fake.emit({ chat: 42, platformUserId: "u", content: "hi" });
  await new Promise((r) => setTimeout(r, 40));
  const before = typed;
  assert.ok(before >= 2);

  fake.rejectExit(new Error("boom"));
  await new Promise((r) => setTimeout(r, 60));
  assert.equal(typed, before, "no pings after polling exit");
});

test("a rejected notifyTyping is swallowed and does not break ingest", async () => {
  const fake = makePlatform();
  fake.platform.notifyTyping = async () => {
    throw new Error("rate limited");
  };
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: fake.platform,
  });
  await channel.onStartBot({ botId: "b1", token: "t" });
  fake.emit({ chat: 42, platformUserId: "u", content: "hi" });
  await new Promise((r) => setImmediate(r));

  // Inbound must still reach a consumer even though the typing ping rejected.
  const iter = channel.inbound(new AbortController().signal)[Symbol.asyncIterator]();
  const next = await iter.next();
  assert.equal(next.done, false);
  assert.equal(next.value.content, "hi");
});

test("explicit StopBot clears the user route (contrast with polling exit)", async () => {
  const fake = makePlatform();
  const channel = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: fake.platform,
  });
  await channel.onStartBot({ botId: "b1", token: "t" });
  fake.emit({ chat: 42, platformUserId: "u1", content: "hi" });
  await channel.onStopBot({ botId: "b1" });
  await channel.onStartBot({ botId: "b1", token: "t2" });
  await channel.onMessage({
    sessionId: "s",
    userId: "test_b1_42_u1",
    content: "should drop",
  });
  assert.equal(fake.calls.sends.length, 0);
});

test("BotChannel exposes onMcpEnvelope only when platform implements onAgentMcpEnvelope", async () => {
  // Codex review regression: BotChannel must conditionally surface
  // round-trip Channel hooks (`onMcpEnvelope`, `onDiagnoseRequested`)
  // based on platform support. The SDK runner reads method presence
  // on the channel object to decide whether to advertise the
  // matching capability — a platform that doesn't implement the
  // hook must NOT have BotChannel claim the capability, otherwise
  // the gateway forwards frames the sidecar can't reply to and the
  // agent times out.
  const fake = makePlatform();
  const without = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: fake.platform,
  });
  assert.equal(without.onMcpEnvelope, undefined);
  assert.equal(without.onDiagnoseRequested, undefined);

  let mcpCalled = false;
  let diagCalled = false;
  const platformWithHooks = {
    ...fake.platform,
    async onAgentMcpEnvelope(_tunnelId, _payload, _reply) {
      mcpCalled = true;
    },
    async onAgentDiagnoseRequested(_req) {
      diagCalled = true;
      return [];
    },
  };
  const withHooks = new BotChannel({
    channelType: "test",
    logger: stubLogger(),
    platform: platformWithHooks,
  });
  assert.equal(typeof withHooks.onMcpEnvelope, "function");
  assert.equal(typeof withHooks.onDiagnoseRequested, "function");

  // Forward call confirms the BotChannel methods actually route to
  // the platform's hooks (not just declared as no-ops).
  await withHooks.onMcpEnvelope("tunnel-x", new Uint8Array([1]), {
    async send() {},
  });
  assert.equal(mcpCalled, true);
  await withHooks.onDiagnoseRequested({ botId: "any" });
  assert.equal(diagCalled, true);
});
