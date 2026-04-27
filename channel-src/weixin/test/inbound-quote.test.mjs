// Cover the new `extractPlainText` features added in P4: ref_msg
// quoting and voice-to-text fallback. The legacy text-only behaviour
// is already covered by `inbound.test.mjs`; these are the additions.

import test from "node:test";
import assert from "node:assert/strict";

import { extractPlainText } from "../dist/messaging/inbound.js";
import { MessageItemType } from "../dist/api/types.js";

test("ref_msg with title + text decorates the body with [引用: ...]", () => {
  const msg = {
    item_list: [
      {
        type: MessageItemType.TEXT,
        text_item: { text: "好的" },
        ref_msg: {
          title: "前言",
          message_item: {
            type: MessageItemType.TEXT,
            text_item: { text: "原话" },
          },
        },
      },
    ],
  };
  assert.equal(extractPlainText(msg), "[引用: 前言 | 原话]\n好的");
});

test("ref_msg pointing at media is NOT inlined into the text body", () => {
  // Quoted media surfaces via `findDownloadableMedia`, not the text.
  // The user's new text passes through unchanged.
  const msg = {
    item_list: [
      {
        type: MessageItemType.TEXT,
        text_item: { text: "see attached" },
        ref_msg: {
          message_item: {
            type: MessageItemType.IMAGE,
            image_item: { media: { encrypt_query_param: "x" } },
          },
        },
      },
    ],
  };
  assert.equal(extractPlainText(msg), "see attached");
});

test("voice with server-side STT lands as text (no media handling)", () => {
  const msg = {
    item_list: [
      {
        type: MessageItemType.VOICE,
        voice_item: { media: { encrypt_query_param: "x" }, text: "听写结果" },
      },
    ],
  };
  assert.equal(extractPlainText(msg), "听写结果");
});

test("STT only fires when the text body is otherwise empty", () => {
  // A user typing a caption should win over a stray voice STT in
  // the same message — we don't want to clobber explicit text.
  const msg = {
    item_list: [
      { type: MessageItemType.TEXT, text_item: { text: "caption" } },
      {
        type: MessageItemType.VOICE,
        voice_item: { text: "should be ignored" },
      },
    ],
  };
  assert.equal(extractPlainText(msg), "caption");
});

test("ref_msg + STT compose: quote prefix on the recognised speech", () => {
  const msg = {
    item_list: [
      {
        type: MessageItemType.TEXT,
        text_item: { text: "" },
        ref_msg: {
          title: "前情",
          message_item: { type: MessageItemType.TEXT, text_item: { text: "上一句" } },
        },
      },
      {
        type: MessageItemType.VOICE,
        voice_item: { text: "好的" },
      },
    ],
  };
  assert.equal(extractPlainText(msg), "[引用: 前情 | 上一句]\n好的");
});
