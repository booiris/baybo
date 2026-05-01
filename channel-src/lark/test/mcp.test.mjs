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

function encodeJson(obj) {
  return TEXT_ENCODER.encode(JSON.stringify(obj));
}

/**
 * Wait until `predicate` returns truthy, polling every microtask. The
 * MCP SDK's protocol layer flushes responses asynchronously via the
 * transport's `onmessage` queue, so the reply may not have landed by
 * the time `accept` returns.
 */
async function waitFor(predicate, label, ticks = 50) {
  for (let i = 0; i < ticks; i++) {
    if (predicate()) return;
    await new Promise((r) => setImmediate(r));
  }
  throw new Error(`waitFor: ${label} did not become true within ${ticks} ticks`);
}

function stubChannel(getChatInfoImpl) {
  return {
    botIdentity: { name: "AuraBot", openId: "ou_bot" },
    async getChatInfo(chatId) {
      return getChatInfoImpl(chatId);
    },
  };
}

async function initialize(server, reply, tunnelId = "tunnel-init") {
  const initReq = encodeJson({
    jsonrpc: "2.0",
    id: 1,
    method: "initialize",
    params: {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "test-client", version: "0.0.1" },
    },
  });
  await server.accept(tunnelId, initReq, reply.handle);
  await waitFor(() => reply.sent.length >= 1, "initialize reply");
  // Send the standard `notifications/initialized` so the SDK
  // unblocks subsequent tool calls.
  await server.accept(
    tunnelId,
    encodeJson({
      jsonrpc: "2.0",
      method: "notifications/initialized",
    }),
    reply.handle,
  );
}

test("LarkMcpServer: tools/list advertises feishu_get_chat_info", async () => {
  let resolverCalls = 0;
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => {
      resolverCalls += 1;
      return null;
    },
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-list",
    encodeJson({ jsonrpc: "2.0", id: 2, method: "tools/list" }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "tools/list reply");

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 2);
  assert.ok(last.result, `expected result, got ${JSON.stringify(last)}`);
  const tools = last.result.tools;
  assert.equal(tools.length, 1);
  assert.equal(tools[0].name, "feishu_get_chat_info");
  // tools/list never resolves a channel — the resolver only fires on
  // tools/call.
  assert.equal(resolverCalls, 0);

  await server.shutdown();
});

test("LarkMcpServer: tools/call routes through getChatInfo", async () => {
  let receivedChatId = "";
  const channel = stubChannel(async (chatId) => {
    receivedChatId = chatId;
    return {
      chatId,
      name: "Engineering",
      chatType: "group",
      memberCount: 42,
    };
  });

  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => channel,
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-call",
    encodeJson({
      jsonrpc: "2.0",
      id: 3,
      method: "tools/call",
      params: {
        name: "feishu_get_chat_info",
        arguments: { chat_id: "oc_demo" },
      },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "tools/call reply");

  assert.equal(receivedChatId, "oc_demo");
  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 3);
  assert.ok(last.result, `expected result, got ${JSON.stringify(last)}`);
  assert.equal(last.result.isError, undefined);
  const text = last.result.content[0].text;
  const parsed = JSON.parse(text);
  assert.equal(parsed.chatId, "oc_demo");
  assert.equal(parsed.memberCount, 42);

  await server.shutdown();
});

test("LarkMcpServer: tools/call surfaces errors as isError replies", async () => {
  const channel = stubChannel(async () => {
    throw new Error("permission_denied: bot not in chat");
  });
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => channel,
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-err",
    encodeJson({
      jsonrpc: "2.0",
      id: 4,
      method: "tools/call",
      params: {
        name: "feishu_get_chat_info",
        arguments: { chat_id: "oc_unreachable" },
      },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "tools/call error reply");

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 4);
  assert.equal(last.result.isError, true);
  assert.match(last.result.content[0].text, /permission_denied/);

  await server.shutdown();
});

test("LarkMcpServer: tools/call with no live bot returns a tool error", async () => {
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => null,
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-no-bot",
    encodeJson({
      jsonrpc: "2.0",
      id: 5,
      method: "tools/call",
      params: {
        name: "feishu_get_chat_info",
        arguments: { chat_id: "oc_x" },
      },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "tools/call no-bot reply");

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 5);
  assert.equal(last.result.isError, true);
  assert.match(last.result.content[0].text, /no Lark bot is currently connected/);

  await server.shutdown();
});

test("LarkMcpServer: distinct tunnel_ids get independent server sessions", async () => {
  const channel = stubChannel(async (chatId) => ({ chatId, name: "X" }));
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => channel,
  });
  // Each tunnel gets its own initialize handshake — they're truly
  // independent sessions over the same WS connection.
  const a = captureReply();
  const b = captureReply();
  await initialize(server, a, "tunnel-a");
  await initialize(server, b, "tunnel-b");

  // List on tunnel A doesn't show up on tunnel B's reply stream.
  const aBefore = a.sent.length;
  await server.accept(
    "tunnel-a",
    encodeJson({ jsonrpc: "2.0", id: 7, method: "tools/list" }),
    a.handle,
  );
  await waitFor(() => a.sent.length > aBefore, "tunnel-a reply");
  // Sanity: B is unaffected. Notifications don't ack, so tunnel-b's
  // reply stream has exactly the one initialize result.
  assert.equal(b.sent.length, 1, "tunnel-b should have only its initialize ack");

  await server.shutdown();
});
