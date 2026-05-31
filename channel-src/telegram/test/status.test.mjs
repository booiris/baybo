// Cover the status-message hooks: sendStatusMessage posts plain text
// and returns the message_id; editStatusMessage edits in place and maps
// a Telegram 429 to StatusRateLimited so the SDK editor backs off. The
// grammy `Bot` is faked with a recording api so the test is offline.

import test from "node:test";
import assert from "node:assert/strict";

import { StatusRateLimited } from "@aura/channel-sdk";
import { GrammyError } from "grammy";

import { TelegramPlatform } from "../dist/platform.js";

function fakeBot(overrides = {}) {
  const calls = { sends: [], edits: [] };
  return {
    calls,
    bot: {
      api: {
        sendMessage:
          overrides.sendMessage ??
          (async (chatId, text, opts) => {
            calls.sends.push({ chatId, text, opts });
            return { message_id: 7 };
          }),
        editMessageText:
          overrides.editMessageText ??
          (async (chatId, messageId, text, opts) => {
            calls.edits.push({ chatId, messageId, text, opts });
            return true;
          }),
      },
    },
  };
}

const stubLogger = () => ({ debug() {}, info() {}, warn() {}, error() {} });

function grammyError(errorCode, description, parameters) {
  return new GrammyError(
    `Call to 'editMessageText' failed!`,
    { ok: false, error_code: errorCode, description, ...(parameters ? { parameters } : {}) },
    "editMessageText",
    {},
  );
}

test("sendStatusMessage posts plain text (no parse_mode) and returns the message_id", async () => {
  const { bot, calls } = fakeBot();
  const platform = new TelegramPlatform(stubLogger());
  const id = await platform.sendStatusMessage(bot, { chatId: 100 }, "🔧 bash · ⏱ 8s");
  assert.equal(id, 7);
  assert.equal(calls.sends.length, 1);
  assert.equal(calls.sends[0].chatId, 100);
  assert.equal(calls.sends[0].text, "🔧 bash · ⏱ 8s");
  assert.equal(calls.sends[0].opts?.parse_mode, undefined, "no MarkdownV2 on the status bubble");
});

test("sendStatusMessage forwards the forum thread id", async () => {
  const { bot, calls } = fakeBot();
  const platform = new TelegramPlatform(stubLogger());
  await platform.sendStatusMessage(bot, { chatId: 100, threadId: 7 }, "…");
  assert.equal(calls.sends[0].opts?.message_thread_id, 7);
});

test("editStatusMessage edits in place by (chatId, message_id), no thread opt", async () => {
  const { bot, calls } = fakeBot();
  const platform = new TelegramPlatform(stubLogger());
  await platform.editStatusMessage(bot, { chatId: 100, threadId: 7 }, 55, "✅ done · 2m12s");
  assert.equal(calls.edits.length, 1);
  assert.equal(calls.edits[0].chatId, 100);
  assert.equal(calls.edits[0].messageId, 55);
  assert.equal(calls.edits[0].text, "✅ done · 2m12s");
  // message_id is chat-global; the thread option must not leak in.
  assert.equal(calls.edits[0].opts, undefined);
});

test("a string message id is coerced to a number", async () => {
  const { bot, calls } = fakeBot();
  const platform = new TelegramPlatform(stubLogger());
  await platform.editStatusMessage(bot, { chatId: 1 }, "77", "x");
  assert.strictEqual(calls.edits[0].messageId, 77);
});

test("editStatusMessage maps a 429 to StatusRateLimited(retry_after * 1000)", async () => {
  const { bot } = fakeBot({
    editMessageText: async () => {
      throw grammyError(429, "Too Many Requests: retry after 3", { retry_after: 3 });
    },
  });
  const platform = new TelegramPlatform(stubLogger());
  await assert.rejects(
    () => platform.editStatusMessage(bot, { chatId: 1 }, 9, "x"),
    (err) => {
      assert.ok(err instanceof StatusRateLimited);
      assert.equal(err.retryAfterMs, 3000);
      return true;
    },
  );
});

test("a 429 without retry_after defaults to a 1s backoff", async () => {
  const { bot } = fakeBot({
    editMessageText: async () => {
      throw grammyError(429, "Too Many Requests");
    },
  });
  const platform = new TelegramPlatform(stubLogger());
  await assert.rejects(
    () => platform.editStatusMessage(bot, { chatId: 1 }, 9, "x"),
    (err) => err instanceof StatusRateLimited && err.retryAfterMs === 1000,
  );
});

test("editStatusMessage rethrows non-429 errors unchanged", async () => {
  const { bot } = fakeBot({
    editMessageText: async () => {
      throw grammyError(400, "Bad Request: message to edit not found");
    },
  });
  const platform = new TelegramPlatform(stubLogger());
  await assert.rejects(
    () => platform.editStatusMessage(bot, { chatId: 1 }, 9, "x"),
    (err) => err instanceof GrammyError && !(err instanceof StatusRateLimited),
  );
});
