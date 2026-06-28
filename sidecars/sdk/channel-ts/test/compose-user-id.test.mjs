// `composeBayboUserId` is the single source of truth for the canonical
// baybo user identity — `BotChannel.ingest` uses it for the inbound
// `Frame::Message.user_id`, and platform code (telegram / weixin /
// future channels) must use it for the `x-baybo-user-id` header on
// blob uploads, otherwise the gateway pairing gate sees two different
// identities for the same physical user and the upload lands in
// `pending` even when the chat is already approved.

import test from "node:test";
import assert from "node:assert/strict";

import { BotChannel, composeBayboUserId, defaultChatKey } from "../dist/bot.js";

function stubLogger() {
  return { debug() {}, info() {}, warn() {}, error() {} };
}

test("primitive ChatId: <channel>_<bot>_<chat>_<user>", () => {
  assert.equal(
    composeBayboUserId("telegram", "b1", 42, "u1"),
    "telegram_b1_42_u1",
  );
});

test("string ChatId passes through verbatim", () => {
  assert.equal(
    composeBayboUserId("weixin", "acct", "wxuser", "wxuser"),
    "weixin_acct_wxuser_wxuser",
  );
});

test("structured ChatId: keys serialize sorted, key=value joined by &", () => {
  // `{chatId: 42, threadId: 1}` and `{chatId: 42, threadId: 2}` must
  // produce different ids so a same-user multi-topic supergroup stays
  // session-isolated. Order-independent: same input shape always
  // produces the same id regardless of property declaration order.
  assert.equal(
    composeBayboUserId("test", "b1", { chatId: 42, threadId: 1 }, "u"),
    "test_b1_chatId=42&threadId=1_u",
  );
  assert.equal(
    composeBayboUserId("test", "b1", { threadId: 1, chatId: 42 }, "u"),
    "test_b1_chatId=42&threadId=1_u",
  );
  assert.equal(
    composeBayboUserId("test", "b1", { chatId: 42 }, "u"),
    "test_b1_chatId=42_u",
  );
});

test("custom chatKey override is applied (mirrors weixin's `chat.toUserId`)", () => {
  // Weixin's chatKey is just `chat.toUserId`, not the default
  // `toUserId=<id>` form. Platforms that override `BotPlatform.chatKey`
  // must thread that override through `composeBayboUserId` so the
  // upload identity matches the inbound text-frame identity.
  const id = composeBayboUserId(
    "weixin",
    "acct1",
    { toUserId: "wxuser" },
    "wxuser",
    (chat) => chat.toUserId,
  );
  assert.equal(id, "weixin_acct1_wxuser_wxuser");
});

test("matches BotChannel.ingest (single source of truth)", async () => {
  // Round-trip proof: emit through the channel, capture the `userId`
  // that lands on the inbound queue, and verify it byte-equals what
  // `composeBayboUserId` produces for the same inputs. If these ever
  // drift, sidecar uploads will silently miss the existing pairing.
  let lastEmit = null;
  const platform = {
    async startBot(cmd, hooks) {
      lastEmit = hooks.emit;
      await hooks.attach(cmd.botId);
      return { handle: cmd.botId, waitForExit: new Promise(() => {}) };
    },
    async stopBot() {},
    async sendText() {},
  };
  const channel = new BotChannel({
    channelType: "telegram",
    logger: stubLogger(),
    platform,
  });
  await channel.onStartBot({ botId: "8405546650", token: "t" });

  const it = channel.inbound(new AbortController().signal)[Symbol.asyncIterator]();
  lastEmit({
    chat: { chatId: 8758857303 },
    platformUserId: "8758857303",
    content: "hi",
  });
  const { value } = await it.next();
  assert.equal(value.userId, "telegram_8405546650_chatId=8758857303_8758857303");
  // `composeBayboUserId` MUST produce the exact same value for the
  // same inputs — that's the contract a sidecar's blob upload relies
  // on to land in the right pairing identity.
  assert.equal(
    composeBayboUserId(
      "telegram",
      "8405546650",
      { chatId: 8758857303 },
      "8758857303",
    ),
    value.userId,
  );

  await channel.onStop();
});

test("defaultChatKey is exported and matches the `<key>=<value>&…` shape", () => {
  // Smoke test: the helper is reachable from the public surface so
  // platform code can use it directly when it needs the chatKey
  // alone (e.g. logging / debug attribution) without composing the
  // full user id.
  assert.equal(defaultChatKey({ chatId: 1, threadId: 2 }), "chatId=1&threadId=2");
  assert.equal(defaultChatKey(42), "42");
});

test("defaultChatKey discriminates nested object values instead of collapsing to [object Object]", () => {
  // Off-contract input (BotPlatform.chatKey docs say primitives /
  // plain-object-of-primitives) used to collide every chat into one
  // bucket via String({...}) -> "[object Object]". Now JSON-stringifies
  // the nested value so distinct shapes still get distinct keys.
  const a = defaultChatKey({ chat: { x: 1 } });
  const b = defaultChatKey({ chat: { x: 2 } });
  assert.notEqual(a, b, "distinct nested values must produce distinct keys");
  assert.ok(!a.includes("[object Object]"), "no [object Object] collision marker");
});
