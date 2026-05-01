import test from "node:test";
import assert from "node:assert/strict";

import { LarkMcpServer } from "../dist/mcp/server.js";

const noopLogger = {
  debug() {},
  info() {},
  warn() {},
  error() {},
};

const TEXT_DECODER = new TextDecoder("utf-8", { fatal: false });
const TEXT_ENCODER = new TextEncoder();

function captureReply() {
  const sent = [];
  return {
    sent,
    handle: {
      async send(payload) {
        sent.push(payload);
      },
    },
  };
}

function decodeJson(bytes) {
  return JSON.parse(TEXT_DECODER.decode(bytes));
}

test("LarkMcpServer: well-formed request gets a 'method not found' error reply", async () => {
  const server = new LarkMcpServer({ logger: noopLogger });
  const { sent, handle } = captureReply();

  const req = TEXT_ENCODER.encode(
    JSON.stringify({
      jsonrpc: "2.0",
      id: 7,
      method: "tools/list",
      params: {},
    }),
  );
  await server.handle(req, handle);

  assert.equal(sent.length, 1);
  const reply = decodeJson(sent[0]);
  assert.equal(reply.jsonrpc, "2.0");
  assert.equal(reply.id, 7);
  assert.equal(reply.error.code, -32601);
  assert.match(reply.error.message, /not yet wired/);
});

test("LarkMcpServer: notification (no id) is dropped silently", async () => {
  const server = new LarkMcpServer({ logger: noopLogger });
  const { sent, handle } = captureReply();

  await server.handle(
    TEXT_ENCODER.encode(
      JSON.stringify({
        jsonrpc: "2.0",
        method: "notifications/initialized",
      }),
    ),
    handle,
  );

  assert.equal(sent.length, 0);
});

test("LarkMcpServer: malformed json drops without throwing", async () => {
  const server = new LarkMcpServer({ logger: noopLogger });
  const { sent, handle } = captureReply();

  await server.handle(TEXT_ENCODER.encode("not json"), handle);
  assert.equal(sent.length, 0);
});

test("LarkMcpServer: id passes through unchanged for string ids", async () => {
  const server = new LarkMcpServer({ logger: noopLogger });
  const { sent, handle } = captureReply();

  await server.handle(
    TEXT_ENCODER.encode(
      JSON.stringify({ jsonrpc: "2.0", id: "abc-123", method: "x" }),
    ),
    handle,
  );

  const reply = decodeJson(sent[0]);
  assert.equal(reply.id, "abc-123");
});

test("LarkMcpServer: missing method still echoes the id with the standard error", async () => {
  // JSON-RPC technically requires `method`, but a malformed request
  // with an `id` should still get a structured error rather than
  // hanging the agent-side caller.
  const server = new LarkMcpServer({ logger: noopLogger });
  const { sent, handle } = captureReply();

  await server.handle(
    TEXT_ENCODER.encode(JSON.stringify({ jsonrpc: "2.0", id: 1 })),
    handle,
  );

  const reply = decodeJson(sent[0]);
  assert.equal(reply.id, 1);
  assert.match(reply.error.message, /<missing>/);
});
