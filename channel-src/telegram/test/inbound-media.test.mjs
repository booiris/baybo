// Cover the inbound media picker for the telegram sidecar. The
// downloader itself is exercised end-to-end against a real bot, so
// the unit tests focus on the metadata shape we hand to the gateway.

import test from "node:test";
import assert from "node:assert/strict";

import { pickInboundMedia } from "../dist/media/inbound.js";

test("photo: largest size wins; mime defaults to image/jpeg", () => {
  const msg = {
    photo: [
      { file_id: "small", file_unique_id: "u1", width: 90, height: 90, file_size: 1234 },
      { file_id: "huge", file_unique_id: "u2", width: 1280, height: 1280, file_size: 65536 },
      { file_id: "mid", file_unique_id: "u3", width: 320, height: 320, file_size: 8192 },
    ],
  };
  const m = pickInboundMedia(msg);
  assert.ok(m);
  assert.equal(m.kind, "image");
  assert.equal(m.fileId, "huge");
  assert.equal(m.mimeType, "image/jpeg");
  assert.equal(m.size, 65536);
  assert.equal(m.filename, undefined);
});

test("voice: kind=audio, mime defaults to audio/ogg, no filename", () => {
  const msg = {
    voice: {
      file_id: "v1",
      file_unique_id: "u",
      duration: 4,
      mime_type: "audio/ogg",
      file_size: 2048,
    },
  };
  const m = pickInboundMedia(msg);
  assert.deepEqual(m, {
    kind: "audio",
    fileId: "v1",
    mimeType: "audio/ogg",
    size: 2048,
  });
});

test("voice: missing mime falls back to audio/ogg (telegram convention)", () => {
  const m = pickInboundMedia({
    voice: { file_id: "v1", file_unique_id: "u", duration: 1 },
  });
  assert.equal(m?.mimeType, "audio/ogg");
});

test("audio: filename + mime preserved, kind=audio", () => {
  const msg = {
    audio: {
      file_id: "a1",
      file_unique_id: "u",
      duration: 30,
      mime_type: "audio/mpeg",
      file_name: "song.mp3",
      file_size: 4096,
    },
  };
  assert.deepEqual(pickInboundMedia(msg), {
    kind: "audio",
    fileId: "a1",
    mimeType: "audio/mpeg",
    filename: "song.mp3",
    size: 4096,
  });
});

test("document: kind=file, mime preserved, filename when present", () => {
  const msg = {
    document: {
      file_id: "d1",
      file_unique_id: "u",
      mime_type: "application/pdf",
      file_name: "spec.pdf",
      file_size: 15000,
    },
  };
  assert.deepEqual(pickInboundMedia(msg), {
    kind: "file",
    fileId: "d1",
    mimeType: "application/pdf",
    filename: "spec.pdf",
    size: 15000,
  });
});

test("document: missing mime defaults to application/octet-stream", () => {
  const m = pickInboundMedia({
    document: { file_id: "d1", file_unique_id: "u" },
  });
  assert.equal(m?.kind, "file");
  assert.equal(m?.mimeType, "application/octet-stream");
});

test("video: kind=file, mime preserved, filename optional", () => {
  const m = pickInboundMedia({
    video: {
      file_id: "vid",
      file_unique_id: "u",
      width: 1920,
      height: 1080,
      duration: 10,
      mime_type: "video/mp4",
      file_size: 50_000,
    },
  });
  assert.deepEqual(m, {
    kind: "file",
    fileId: "vid",
    mimeType: "video/mp4",
    size: 50_000,
  });
});

test("priority: photo wins over video/voice/audio/document", () => {
  // Telegram normally only fills one media field, but a forwarded
  // animation sets both `animation` and `document`. Pin the priority
  // to make the dispatch deterministic regardless of upstream tweaks.
  const msg = {
    photo: [{ file_id: "p", file_unique_id: "u", width: 10, height: 10 }],
    video: { file_id: "v", file_unique_id: "u", width: 1, height: 1, duration: 1 },
    voice: { file_id: "vo", file_unique_id: "u", duration: 1 },
    audio: { file_id: "a", file_unique_id: "u", duration: 1 },
    document: { file_id: "d", file_unique_id: "u" },
  };
  assert.equal(pickInboundMedia(msg)?.fileId, "p");
});

test("priority: video > voice when there's no photo", () => {
  const msg = {
    video: { file_id: "v", file_unique_id: "u", width: 1, height: 1, duration: 1 },
    voice: { file_id: "vo", file_unique_id: "u", duration: 1 },
  };
  assert.equal(pickInboundMedia(msg)?.fileId, "v");
});

test("priority: voice > audio when both are present", () => {
  // Edge case: a forwarded voice note that the client also tags as a
  // music track. Voice should still win because it represents the
  // user's actual interaction (push-to-talk).
  const msg = {
    voice: { file_id: "vo", file_unique_id: "u", duration: 1 },
    audio: { file_id: "a", file_unique_id: "u", duration: 1 },
  };
  assert.equal(pickInboundMedia(msg)?.kind, "audio");
  assert.equal(pickInboundMedia(msg)?.fileId, "vo");
});

test("priority: audio > document when there's no voice", () => {
  const msg = {
    audio: { file_id: "a", file_unique_id: "u", duration: 1 },
    document: { file_id: "d", file_unique_id: "u" },
  };
  assert.equal(pickInboundMedia(msg)?.fileId, "a");
});

test("text-only message returns null (caller falls back to text path)", () => {
  assert.equal(pickInboundMedia({ text: "hi" }), null);
});

test("unsupported types (sticker / location / contact) return null", () => {
  assert.equal(
    pickInboundMedia({ sticker: { file_id: "s", file_unique_id: "u", width: 1, height: 1 } }),
    null,
  );
  assert.equal(pickInboundMedia({ location: { latitude: 0, longitude: 0 } }), null);
});

test("empty photo array is treated as no media", () => {
  // Defensive: telegram never sends `photo: []`, but the picker must
  // not blow up on a sanitised inbound webhook either.
  assert.equal(pickInboundMedia({ photo: [] }), null);
});
