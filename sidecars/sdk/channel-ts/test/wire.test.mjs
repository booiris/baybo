// Wire-level round-trip tests for attachment-bearing frames. These
// exist to catch the kind of regression Codex flagged in the
// adversarial review: a Rust-side u64 `size` produces a TS `bigint`
// that the default `@msgpack/msgpack` encoder rejects at runtime.
// Keep these as plain encode/decode assertions — no WebSocket or
// runtime dependency — so a typing/encoding mismatch surfaces in the
// fastest test we have.

import test from "node:test";
import assert from "node:assert/strict";

import { decodeFrame, encodeFrame } from "../dist/wire.js";

test("encodeFrame round-trips a Message with attachments", () => {
  const frame = {
    kind: "message",
    content: "look at this",
    session_id: "sess-1",
    user_id: "u-1",
    channel_type: "weixin",
    bot_id: "prod-bot",
    attachments: [
      {
        kind: "image",
        blob_id: `sha256:${"0".repeat(64)}`,
        mime_type: "image/png",
        size: 1024,
      },
      {
        kind: "file",
        blob_id: `sha256:${"1".repeat(64)}`,
        mime_type: "application/pdf",
        size: 4096,
        filename: "report.pdf",
      },
    ],
  };

  const bytes = encodeFrame(frame);
  const decoded = decodeFrame(bytes);

  assert.equal(decoded.kind, "message");
  assert.equal(decoded.content, frame.content);
  assert.equal(decoded.attachments.length, 2);
  assert.equal(decoded.attachments[0].kind, "image");
  // size must round-trip as a number — a bigint here means we drifted
  // back to u64 and broke wire compatibility with the default encoder.
  assert.equal(typeof decoded.attachments[0].size, "number");
  assert.equal(decoded.attachments[0].size, 1024);
  assert.equal(decoded.attachments[1].filename, "report.pdf");
});

test("encodeFrame accepts a Message without attachments key", () => {
  // Sidecars predating media support send only the legacy fields.
  // The wire must accept that shape and decode it cleanly — a
  // gateway that demands an explicit empty list would force every
  // existing sidecar to recompile.
  const frame = {
    kind: "message",
    content: "hi",
    session_id: "sess",
    user_id: "u",
    channel_type: "tui",
  };
  const decoded = decodeFrame(encodeFrame(frame));
  assert.equal(decoded.kind, "message");
  assert.equal(decoded.content, "hi");
});

test("encodeFrame round-trips a Message with platform_msg_id", () => {
  const frame = {
    kind: "message",
    content: "hi",
    session_id: "",
    user_id: "u-1",
    channel_type: "weixin",
    bot_id: "prod-bot",
    platform_msg_id: "msg-42",
  };
  const decoded = decodeFrame(encodeFrame(frame));
  assert.equal(decoded.kind, "message");
  assert.equal(decoded.platform_msg_id, "msg-42");
});
