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
