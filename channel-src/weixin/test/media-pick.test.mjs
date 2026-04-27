// Coverage for `findDownloadableMedia` — the priority logic
// (IMAGE > VIDEO > FILE > VOICE) and the ref_msg fallback are the
// only piece of the inbound media path that runs without any actual
// network or filesystem state, so it's the cheapest place to pin
// the expected behaviour.

import test from "node:test";
import assert from "node:assert/strict";

import { findDownloadableMedia, isMediaItem } from "../dist/media/media-download.js";
import { MessageItemType } from "../dist/api/types.js";

const downloadable = (type, key) => ({
  type,
  [key]: { media: { encrypt_query_param: "encparam" } },
});

test("isMediaItem covers image / video / file / voice only", () => {
  assert.equal(isMediaItem({ type: MessageItemType.IMAGE }), true);
  assert.equal(isMediaItem({ type: MessageItemType.VIDEO }), true);
  assert.equal(isMediaItem({ type: MessageItemType.FILE }), true);
  assert.equal(isMediaItem({ type: MessageItemType.VOICE }), true);
  assert.equal(isMediaItem({ type: MessageItemType.TEXT }), false);
  assert.equal(isMediaItem({ type: MessageItemType.NONE }), false);
});

test("priority: IMAGE wins over VIDEO/FILE/VOICE in the same item_list", () => {
  const msg = {
    item_list: [
      downloadable(MessageItemType.VIDEO, "video_item"),
      downloadable(MessageItemType.FILE, "file_item"),
      downloadable(MessageItemType.IMAGE, "image_item"),
      downloadable(MessageItemType.VOICE, "voice_item"),
    ],
  };
  assert.equal(findDownloadableMedia(msg)?.type, MessageItemType.IMAGE);
});

test("priority: VIDEO wins over FILE/VOICE when no IMAGE", () => {
  const msg = {
    item_list: [
      downloadable(MessageItemType.FILE, "file_item"),
      downloadable(MessageItemType.VOICE, "voice_item"),
      downloadable(MessageItemType.VIDEO, "video_item"),
    ],
  };
  assert.equal(findDownloadableMedia(msg)?.type, MessageItemType.VIDEO);
});

test("VOICE with server-side STT text is skipped (handled via inbound text path)", () => {
  const msg = {
    item_list: [
      {
        type: MessageItemType.VOICE,
        voice_item: { media: { encrypt_query_param: "x" }, text: "STT result" },
      },
    ],
  };
  assert.equal(findDownloadableMedia(msg), null);
});

test("ref_msg media is picked up when the main item_list has none", () => {
  const msg = {
    item_list: [
      {
        type: MessageItemType.TEXT,
        text_item: { text: "hi" },
        ref_msg: {
          message_item: downloadable(MessageItemType.IMAGE, "image_item"),
        },
      },
    ],
  };
  const found = findDownloadableMedia(msg);
  assert.equal(found?.type, MessageItemType.IMAGE);
});

test("returns null when nothing is downloadable", () => {
  const msg = {
    item_list: [
      { type: MessageItemType.TEXT, text_item: { text: "plain" } },
    ],
  };
  assert.equal(findDownloadableMedia(msg), null);
});

test("returns null when media item has neither encrypt_query_param nor full_url", () => {
  const msg = {
    item_list: [{ type: MessageItemType.IMAGE, image_item: { media: {} } }],
  };
  assert.equal(findDownloadableMedia(msg), null);
});
