// Cover the outbound media dispatcher — kind + mime → which
// `bot.api.sendXxx` runs. The grammy `Bot` is faked with a recording
// proxy so the test is fully offline.

import test from "node:test";
import assert from "node:assert/strict";
import { InputFile } from "grammy";

import { sendTelegramAttachment } from "../dist/media/outbound.js";

function fakeBot() {
  const calls = [];
  const record = (method) => async (chatId, file, opts) => {
    calls.push({ method, chatId, file, opts });
    return { message_id: calls.length };
  };
  return {
    bot: {
      api: {
        sendPhoto: record("sendPhoto"),
        sendVideo: record("sendVideo"),
        sendVoice: record("sendVoice"),
        sendAudio: record("sendAudio"),
        sendDocument: record("sendDocument"),
      },
    },
    calls,
  };
}

const stubLogger = () => ({ debug() {}, info() {}, warn() {}, error() {} });

const bytes = (s) => Buffer.from(s, "utf-8");

test("image kind → sendPhoto with caption + thread", async () => {
  const { bot, calls } = fakeBot();
  await sendTelegramAttachment(
    bot,
    { chatId: 100, threadId: 7 },
    { kind: "image", blob_id: "b", mime_type: "image/jpeg", size: 9 },
    bytes("imgdata"),
    "look at this",
    stubLogger(),
  );
  assert.equal(calls.length, 1);
  assert.equal(calls[0].method, "sendPhoto");
  assert.equal(calls[0].chatId, 100);
  assert.ok(calls[0].file instanceof InputFile);
  assert.equal(calls[0].opts.caption, "look at this");
  assert.equal(calls[0].opts.parse_mode, "MarkdownV2");
  assert.equal(calls[0].opts.message_thread_id, 7);
});

test("markdown caption is converted to MarkdownV2", async () => {
  const { bot, calls } = fakeBot();
  await sendTelegramAttachment(
    bot,
    { chatId: 1 },
    { kind: "image", blob_id: "b", mime_type: "image/jpeg", size: 1 },
    bytes("..."),
    "v**1.0.0** released",
    stubLogger(),
  );
  assert.equal(calls[0].opts.caption, "v*1\\.0\\.0* released");
  assert.equal(calls[0].opts.parse_mode, "MarkdownV2");
});

test("audio kind w/ audio/ogg → sendVoice (voice-note bubble)", async () => {
  const { bot, calls } = fakeBot();
  await sendTelegramAttachment(
    bot,
    { chatId: 1 },
    { kind: "audio", blob_id: "b", mime_type: "audio/ogg", size: 1 },
    bytes("..."),
    "",
    stubLogger(),
  );
  assert.equal(calls[0].method, "sendVoice");
  assert.equal(calls[0].opts.caption, undefined);
  assert.equal(calls[0].opts.message_thread_id, undefined);
});

test("audio kind w/ audio/mpeg → sendAudio (music)", async () => {
  const { bot, calls } = fakeBot();
  await sendTelegramAttachment(
    bot,
    { chatId: 1 },
    { kind: "audio", blob_id: "b", mime_type: "audio/mpeg", size: 1, filename: "song.mp3" },
    bytes("..."),
    "",
    stubLogger(),
  );
  assert.equal(calls[0].method, "sendAudio");
});

test("file kind w/ video/* → sendVideo (streamable player)", async () => {
  const { bot, calls } = fakeBot();
  await sendTelegramAttachment(
    bot,
    { chatId: 1 },
    { kind: "file", blob_id: "b", mime_type: "video/mp4", size: 1, filename: "clip.mp4" },
    bytes("..."),
    "rolling",
    stubLogger(),
  );
  assert.equal(calls[0].method, "sendVideo");
  assert.equal(calls[0].opts.caption, "rolling");
});

test("file kind w/ application/pdf → sendDocument", async () => {
  const { bot, calls } = fakeBot();
  await sendTelegramAttachment(
    bot,
    { chatId: 42 },
    { kind: "file", blob_id: "b", mime_type: "application/pdf", size: 1, filename: "spec.pdf" },
    bytes("..."),
    "",
    stubLogger(),
  );
  assert.equal(calls[0].method, "sendDocument");
});

test("empty caption is omitted (no caption key in opts)", async () => {
  const { bot, calls } = fakeBot();
  await sendTelegramAttachment(
    bot,
    { chatId: 1 },
    { kind: "image", blob_id: "b", mime_type: "image/jpeg", size: 1 },
    bytes("..."),
    "",
    stubLogger(),
  );
  // Telegram clients render `caption: ""` as a blank message bubble
  // under the photo, so the dispatcher must drop the key entirely
  // when there is no text instead of forwarding an empty string.
  assert.ok(!("caption" in calls[0].opts));
});

test("caption above 1024 chars is truncated codepoint-safely", async () => {
  const { bot, calls } = fakeBot();
  // Build a payload that exceeds the cap with multibyte codepoints
  // — naive byte-slicing would split a surrogate pair and produce an
  // invalid UTF-16 string the bot API rejects.
  const big = "🐳".repeat(2000);
  await sendTelegramAttachment(
    bot,
    { chatId: 1 },
    { kind: "image", blob_id: "b", mime_type: "image/jpeg", size: 1 },
    bytes("..."),
    big,
    stubLogger(),
  );
  const caption = calls[0].opts.caption;
  // 1024 codepoints exactly. `[...str].length` counts codepoints, so
  // the truncated value should round-trip to the same length.
  assert.equal([...caption].length, 1024);
  assert.ok(caption.endsWith("🐳"));
});

test("filename is forwarded to InputFile (preserved across send)", async () => {
  const { bot, calls } = fakeBot();
  await sendTelegramAttachment(
    bot,
    { chatId: 1 },
    {
      kind: "file",
      blob_id: "b",
      mime_type: "application/pdf",
      size: 1,
      filename: "report.pdf",
    },
    bytes("..."),
    "",
    stubLogger(),
  );
  // grammy's InputFile exposes the filename it'll send to multipart
  // upload — the dispatcher must pass `att.filename` through verbatim
  // so the user sees the original name on the receiving end.
  assert.equal(calls[0].file.filename, "report.pdf");
});

test("missing filename gets a kind-aware default extension", async () => {
  const { bot, calls } = fakeBot();
  await sendTelegramAttachment(
    bot,
    { chatId: 1 },
    { kind: "image", blob_id: "b", mime_type: "image/png", size: 1 },
    bytes("..."),
    "",
    stubLogger(),
  );
  // No explicit filename, so the dispatcher synthesises one from the
  // mime — `image.png` is what we want to land on telegram clients.
  assert.equal(calls[0].file.filename, "image.png");
});

test("an image kind sendPhoto can't take (svg) ships as a document", async () => {
  // The wire's `image` kind is a rendering hint — a transcript draws an SVG
  // inline — but `sendPhoto` rejects it. Falling back to sendDocument keeps
  // the file instead of losing it to a 400.
  const { bot, calls } = fakeBot();
  await sendTelegramAttachment(
    bot,
    { chatId: 100 },
    {
      kind: "image",
      blob_id: "b",
      mime_type: "image/svg+xml",
      size: 9,
      filename: "diagram.svg",
    },
    bytes("<svg/>"),
    "",
    stubLogger(),
  );
  assert.equal(calls.length, 1);
  assert.equal(calls[0].method, "sendDocument");
});

test("every raster sendPhoto accepts still takes the photo path", async () => {
  for (const mime of [
    "image/jpeg",
    "image/png",
    "image/gif",
    "image/webp",
    "image/bmp",
  ]) {
    const { bot, calls } = fakeBot();
    await sendTelegramAttachment(
      bot,
      { chatId: 100 },
      { kind: "image", blob_id: "b", mime_type: mime, size: 9 },
      bytes("raster"),
      "",
      stubLogger(),
    );
    assert.equal(calls[0].method, "sendPhoto", mime);
  }
});
