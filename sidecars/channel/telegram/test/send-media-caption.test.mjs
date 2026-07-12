// A turn's media now rides on the agent's terminal reply, so `sendMedia`
// receives the reply text as the would-be caption. Telegram truncates
// captions at 1024 codepoints, which would silently eat the tail of a long
// answer — so `sendMedia` only captions a reply that fits, and otherwise
// ships the attachments bare plus the full text as its own message.
// The grammy `Bot` and the blob fetch are faked so the test is offline.

import test from "node:test";
import assert from "node:assert/strict";

import { GrammyError } from "grammy";

import { TelegramPlatform } from "../dist/platform.js";
import { TELEGRAM_CAPTION_MAX } from "../dist/media/outbound.js";

const stubLogger = () => ({ debug() {}, info() {}, warn() {}, error() {} });

function parseEntitiesError() {
  return new GrammyError(
    "Call to 'sendDocument' failed!",
    {
      ok: false,
      error_code: 400,
      description: "Bad Request: can't parse entities: byte offset 12",
    },
    "sendDocument",
    {},
  );
}

/** `sendDocument` rejects on the `attempt`-th call (1-indexed) with `err`. */
function fakeBot({ failDocumentOn, error } = {}) {
  const calls = { documents: [], messages: [] };
  let doc = 0;
  return {
    calls,
    bot: {
      api: {
        sendDocument: async (chatId, file, opts) => {
          doc += 1;
          calls.documents.push({ chatId, file, opts });
          if (doc === failDocumentOn) throw error;
        },
        sendMessage: async (chatId, text, opts) => {
          calls.messages.push({ chatId, text, opts });
          return { message_id: 1 };
        },
      },
    },
  };
}

/** `openAttachmentBytes` is `private` in TS only — stub it so no blob
 *  fetch happens and every attachment "downloads" successfully. */
function platformWithBytes() {
  const platform = new TelegramPlatform(stubLogger());
  platform.openAttachmentBytes = async () => new Uint8Array([1, 2, 3]);
  return platform;
}

const chat = { chatId: 42 };
const attachment = {
  kind: "file",
  blob_id: "sha256:ab",
  mime_type: "application/pdf",
  size: 3,
  filename: "report.pdf",
};

test("a caption Telegram can't parse is retried as plain text, so the file still lands", async () => {
  // The caption is the agent's own markdown prose, so it hits the same
  // `telegramify-markdown` output Telegram sometimes rejects. Without a retry
  // the 400 would kill the attachment while the reply text survived.
  const { bot, calls } = fakeBot({
    failDocumentOn: 1,
    error: parseEntitiesError(),
  });
  await platformWithBytes().sendMedia(bot, chat, {
    text: "see *this*",
    attachments: [attachment],
  });

  assert.equal(calls.documents.length, 2, "the send is retried once");
  assert.ok(
    calls.documents[0].opts.parse_mode,
    "first attempt goes out as MarkdownV2",
  );
  assert.ok(
    !calls.documents[1].opts.parse_mode,
    "the retry drops parse_mode",
  );
  assert.equal(
    calls.documents[1].opts.caption,
    "see *this*",
    "the retry carries the raw, unconverted caption",
  );
  assert.equal(calls.messages.length, 0, "no separate text message");
});

test("a non-parse error is not retried and does not resurrect the caption", async () => {
  const { bot, calls } = fakeBot({
    failDocumentOn: 1,
    error: new Error("network down"),
  });
  await platformWithBytes().sendMedia(bot, chat, {
    text: "see this",
    attachments: [attachment],
  });

  assert.equal(calls.documents.length, 1, "no retry on an unrelated failure");
  // The attachment never shipped, so the caption never rode — the reply
  // falls back to a plain text message rather than going silent.
  assert.deepEqual(calls.messages.map((m) => m.text), ["see this"]);
});

test("a reply short enough to caption rides along with the attachment", async () => {
  const { bot, calls } = fakeBot();
  await platformWithBytes().sendMedia(bot, chat, {
    text: "here is the report",
    attachments: [attachment],
  });

  assert.equal(calls.documents.length, 1);
  assert.equal(calls.documents[0].opts.caption, "here is the report");
  assert.equal(calls.messages.length, 0, "no separate text message");
});

test("a reply too long to caption ships whole, as its own message", async () => {
  const { bot, calls } = fakeBot();
  const long = "x".repeat(TELEGRAM_CAPTION_MAX + 1);
  await platformWithBytes().sendMedia(bot, chat, {
    text: long,
    attachments: [attachment],
  });

  assert.equal(calls.documents.length, 1);
  assert.ok(
    !("caption" in calls.documents[0].opts),
    "an over-long reply must not become a truncated caption",
  );
  assert.equal(calls.messages.length, 1);
  assert.equal(calls.messages[0].text.length, long.length, "no text is lost");
});

test("a caption-length reply still ships when every attachment fails to fetch", async () => {
  const { bot, calls } = fakeBot();
  const platform = new TelegramPlatform(stubLogger());
  platform.openAttachmentBytes = async () => null;

  await platform.sendMedia(bot, chat, {
    text: "here is the report",
    attachments: [attachment],
  });

  assert.equal(calls.documents.length, 0);
  assert.equal(calls.messages.length, 1, "the reply must not go silent");
  assert.equal(calls.messages[0].text, "here is the report");
});
