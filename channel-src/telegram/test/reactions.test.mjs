// Cover the working-reaction hooks — set/clear map onto
// `bot.api.setMessageReaction` with the right emoji payload. The grammy
// `Bot` is faked with a recording api so the test is fully offline.

import test from "node:test";
import assert from "node:assert/strict";

import { TelegramPlatform } from "../dist/platform.js";

function fakeBot() {
  const calls = [];
  return {
    bot: {
      api: {
        setMessageReaction: async (chatId, messageId, reaction, opts) => {
          calls.push({ chatId, messageId, reaction, opts });
          return true;
        },
      },
    },
    calls,
  };
}

const stubLogger = () => ({ debug() {}, info() {}, warn() {}, error() {} });

test("setWorkingReaction reacts with 👀 on the target message_id", async () => {
  const { bot, calls } = fakeBot();
  const platform = new TelegramPlatform(stubLogger());
  // threadId must NOT leak into the reaction call — message_id is
  // chat-global, so setMessageReaction takes no thread option.
  await platform.setWorkingReaction(bot, { chatId: 100, threadId: 7 }, 55);
  assert.equal(calls.length, 1);
  assert.equal(calls[0].chatId, 100);
  assert.equal(calls[0].messageId, 55);
  assert.deepEqual(calls[0].reaction, [{ type: "emoji", emoji: "👀" }]);
});

test("clearWorkingReaction removes the reaction with an empty list", async () => {
  const { bot, calls } = fakeBot();
  const platform = new TelegramPlatform(stubLogger());
  await platform.clearWorkingReaction(bot, { chatId: 100 }, 55);
  assert.equal(calls.length, 1);
  assert.equal(calls[0].chatId, 100);
  assert.equal(calls[0].messageId, 55);
  assert.deepEqual(calls[0].reaction, []);
});

test("a string target is coerced to a numeric message_id", async () => {
  const { bot, calls } = fakeBot();
  const platform = new TelegramPlatform(stubLogger());
  await platform.setWorkingReaction(bot, { chatId: 1 }, "77");
  assert.strictEqual(calls[0].messageId, 77);
});
