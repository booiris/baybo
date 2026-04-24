import test from "node:test";
import assert from "node:assert/strict";

import { extractPlainText } from "../dist/messaging/inbound.js";
import { MessageItemType } from "../dist/api/types.js";

test("extractPlainText concatenates every TEXT item in order", () => {
  const msg = {
    item_list: [
      { type: MessageItemType.TEXT, text_item: { text: "hello " } },
      { type: MessageItemType.TEXT, text_item: { text: "world" } },
    ],
  };
  assert.equal(extractPlainText(msg), "hello world");
});

test("extractPlainText ignores image/voice/file/video items", () => {
  const msg = {
    item_list: [
      { type: MessageItemType.IMAGE, image_item: {} },
      { type: MessageItemType.TEXT, text_item: { text: "caption" } },
      { type: MessageItemType.FILE, file_item: { file_name: "a.pdf" } },
      { type: MessageItemType.VOICE, voice_item: {} },
      { type: MessageItemType.VIDEO, video_item: {} },
    ],
  };
  assert.equal(extractPlainText(msg), "caption");
});

test("extractPlainText returns '' for media-only messages", () => {
  const msg = {
    item_list: [
      { type: MessageItemType.IMAGE, image_item: {} },
      { type: MessageItemType.VOICE, voice_item: {} },
    ],
  };
  assert.equal(extractPlainText(msg), "");
});

test("extractPlainText handles missing item_list", () => {
  assert.equal(extractPlainText({}), "");
});

test("extractPlainText skips TEXT items with empty/missing text", () => {
  const msg = {
    item_list: [
      { type: MessageItemType.TEXT, text_item: {} },
      { type: MessageItemType.TEXT, text_item: { text: "" } },
      { type: MessageItemType.TEXT, text_item: { text: "ok" } },
    ],
  };
  assert.equal(extractPlainText(msg), "ok");
});
