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

function stubChannel(getChatInfoImpl, rawApi) {
  return {
    botIdentity: { name: "AuraBot", openId: "ou_bot" },
    async getChatInfo(chatId) {
      return getChatInfoImpl(chatId);
    },
    rawClient: {
      authen: {
        userInfo: {
          async get(payload, opts) {
            if (!rawApi?.authenUserInfoGet) {
              throw new Error("authen.userInfo.get stub not configured");
            }
            return rawApi.authenUserInfoGet(payload, opts);
          },
        },
      },
      contact: {
        v3: {
          user: {
            async get(payload, opts) {
              if (!rawApi?.contactUserGet) {
                throw new Error("contact.v3.user.get stub not configured");
              }
              return rawApi.contactUserGet(payload, opts);
            },
          },
        },
      },
      calendar: {
        calendar: {
          async primary(payload, opts) {
            return rawApi?.calendarPrimary?.(payload, opts) ?? { code: 0, data: {} };
          },
          async list(payload, opts) {
            return rawApi?.calendarList?.(payload, opts) ?? { code: 0, data: {} };
          },
          async get(payload, opts) {
            return rawApi?.calendarGet?.(payload, opts) ?? { code: 0, data: {} };
          },
        },
        freebusy: {
          async list(payload, opts) {
            return rawApi?.freebusyList?.(payload, opts) ?? { code: 0, data: {} };
          },
          async batch(payload, opts) {
            return rawApi?.freebusyBatch?.(payload, opts) ?? { code: 0, data: {} };
          },
        },
      },
      im: {
        v1: {
          chat: {
            async search(payload) {
              if (!rawApi?.chatSearch) {
                throw new Error("chat.search stub not configured");
              }
              return rawApi.chatSearch(payload);
            },
          },
          chatMembers: {
            async get(payload) {
              if (!rawApi?.chatMembersGet) {
                throw new Error("chatMembers.get stub not configured");
              }
              return rawApi.chatMembersGet(payload);
            },
          },
          message: {
            async list(payload) {
              if (!rawApi?.messageList) {
                throw new Error("message.list stub not configured");
              }
              return rawApi.messageList(payload);
            },
            async get(payload) {
              if (!rawApi?.messageGet) {
                throw new Error("message.get stub not configured");
              }
              return rawApi.messageGet(payload);
            },
          },
        },
      },
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

test("LarkMcpServer: tools/list advertises the registered tool surface", async () => {
  let resolverCalls = 0;
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => {
      resolverCalls += 1;
      return { kind: "none" };
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
  const toolNames = last.result.tools.map((t) => t.name).sort();
  assert.deepEqual(toolNames, [
    "feishu_ask_user",
    "feishu_calendar",
    "feishu_freebusy",
    "feishu_freebusy_batch",
    "feishu_get_chat_history",
    "feishu_get_chat_info",
    "feishu_get_message",
    "feishu_get_user",
    "feishu_list_chat_members",
    "feishu_search_chats",
    "feishu_search_user",
    "feishu_who_am_i",
  ]);
  // tools/list never resolves a channel — the resolver only fires on
  // tools/call.
  assert.equal(resolverCalls, 0);

  await server.shutdown();
});

test("LarkMcpServer: tools/call routes through getChatInfo using the resolved chat", async () => {
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
    // Resolver supplies the chat id from the active conversation.
    // The tool no longer accepts `chat_id` in its args (Codex #2):
    // a paired user could otherwise drive the bot to disclose
    // metadata for any chat the bot can access.
    channelResolver: () => ({
      kind: "ok",
      channel,
      chatId: "oc_demo",
      platformUserId: "ou_alice",
    }),
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
        arguments: {},
      },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "tools/call reply");

  // Resolver provided `oc_demo`; that's what reached `getChatInfo`.
  assert.equal(receivedChatId, "oc_demo");
  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 3);
  assert.ok(last.result, `expected result, got ${JSON.stringify(last)}`);
  assert.equal(last.result.isError, undefined);
  const parsed = JSON.parse(last.result.content[0].text);
  assert.equal(parsed.chatId, "oc_demo");
  assert.equal(parsed.memberCount, 42);

  await server.shutdown();
});

test("LarkMcpServer: feishu_get_chat_info ignores LLM-supplied chat_id (cross-chat lookup)", async () => {
  // Codex #2 regression: even if the LLM hallucinated a `chat_id`
  // argument referencing a different chat, the tool must run
  // against the resolver's chat. The MCP SDK's zod schema with
  // empty `inputSchema: {}` rejects extra fields strictly, so the
  // call can't smuggle a chat id at all — but we double-check
  // here by asserting the chat the resolver supplied is the one
  // `getChatInfo` saw.
  let receivedChatId = "";
  const channel = stubChannel(async (chatId) => {
    receivedChatId = chatId;
    return { chatId, name: "ActiveChat", chatType: "group", memberCount: 4 };
  });
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({
      kind: "ok",
      channel,
      chatId: "oc_active_chat",
      platformUserId: "ou_alice",
    }),
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-cross-chat",
    encodeJson({
      jsonrpc: "2.0",
      id: 15,
      method: "tools/call",
      params: {
        name: "feishu_get_chat_info",
        // The LLM tries to override with a private chat the bot
        // happens to see. Either zod rejects (ideal) or the tool
        // ignores the arg and runs on the resolver's chat.
        arguments: { chat_id: "oc_private_secrets" },
      },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "cross-chat reply");

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 15);
  // The tool MUST NOT have queried the malicious chat id. Either:
  //   (a) zod rejected the call → isError true and getChatInfo never ran, or
  //   (b) the schema allowed the extra field but the tool ignored
  //       it → getChatInfo ran on the resolver's chat.
  // Both are safe; we assert the safety invariant directly.
  assert.notEqual(
    receivedChatId,
    "oc_private_secrets",
    "feishu_get_chat_info must not query an LLM-supplied chat id",
  );

  await server.shutdown();
});

test("LarkMcpServer: tools/call surfaces errors as isError replies", async () => {
  const channel = stubChannel(async () => {
    throw new Error("permission_denied: bot not in chat");
  });
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({
      kind: "ok",
      channel,
      chatId: "oc_unreachable",
      platformUserId: "ou_alice",
    }),
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
        arguments: {},
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

test("LarkMcpServer: tools/call with no active conversation returns a tool error", async () => {
  // Post Codex #2: the resolver returns `none` not just for "no
  // bot connected" but also for "no chat context" (e.g. an MCP
  // call comes in before any inbound Feishu message has populated
  // `contextByAuraUser`). Both paths surface the same structured
  // error; the LLM doesn't need to distinguish them.
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({ kind: "none" }),
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
        arguments: {},
      },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "tools/call no-bot reply");

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 5);
  assert.equal(last.result.isError, true);
  assert.match(last.result.content[0].text, /no active Lark conversation/);

  await server.shutdown();
});

test("LarkMcpServer: multi-bot fails closed when no auraBotId is supplied", async () => {
  // Codex review regression: when multiple bots are live, a tool
  // call from bot B's user must NOT silently execute under bot A's
  // credentials and return A's tenant metadata. Slice 2A fails
  // closed; slice 2F lets a caller resolve directly via
  // `_meta.auraBotId`. Without that meta we still fail closed.
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({ kind: "ambiguous", bot_count: 3 }),
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-multi",
    encodeJson({
      jsonrpc: "2.0",
      id: 6,
      method: "tools/call",
      params: {
        name: "feishu_get_chat_info",
        arguments: { chat_id: "oc_x" },
      },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "tools/call ambiguous reply");

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 6);
  assert.equal(last.result.isError, true);
  assert.match(last.result.content[0].text, /multi-bot routing requires an .auraBotId. on the call/);
  assert.match(last.result.content[0].text, /3 bots live/);

  await server.shutdown();
});

test("LarkMcpServer: tools/call routes by _meta.auraBotId in multi-bot mode", async () => {
  // Slice 2F: when the caller supplies `_meta.auraBotId`, the
  // resolver picks that bot directly even with multiple connected.
  // This proves the `auraBotId` field reaches the resolver and the
  // resolver consumes it. The stub records which bot was asked for.
  let resolverInput = null;
  const channel = stubChannel(async (chatId) => ({
    chatId,
    name: "BotB-tenant",
    chatType: "group",
    memberCount: 7,
  }));
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: (input) => {
      resolverInput = input;
      // Mimic LarkPlatform's slice-2F resolver: route to the named
      // bot when present, otherwise fall back to the slice-2A
      // 3-state behaviour. Per Codex #2, `ok` now carries chatId
      // + platformUserId so chat-bound tools don't accept an
      // arbitrary chat id from the LLM.
      if (input.auraBotId === "cli_bot_b") {
        return {
          kind: "ok",
          channel,
          chatId: "oc_b",
          platformUserId: "ou_alice",
        };
      }
      return { kind: "ambiguous", bot_count: 2 };
    },
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-route",
    encodeJson({
      jsonrpc: "2.0",
      id: 9,
      method: "tools/call",
      params: {
        name: "feishu_get_chat_info",
        arguments: { chat_id: "oc_b" },
        _meta: {
          auraBotId: "cli_bot_b",
          auraSessionId: "aura-session-77",
        },
      },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "tools/call routed reply");

  assert.deepEqual(resolverInput, {
    auraBotId: "cli_bot_b",
    auraSessionId: "aura-session-77",
  });

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 9);
  assert.equal(last.result.isError, undefined);
  const parsed = JSON.parse(last.result.content[0].text);
  assert.equal(parsed.name, "BotB-tenant");

  await server.shutdown();
});

test("LarkMcpServer: feishu_ask_user without configured handler surfaces no_context", async () => {
  // The default opts (no askUser) returns no_context so the test
  // path stays clean. Production wires LarkPlatform.askUser.
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({ kind: "none" }),
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-ask-default",
    encodeJson({
      jsonrpc: "2.0",
      id: 11,
      method: "tools/call",
      params: {
        name: "feishu_ask_user",
        arguments: { prompt: "what's your favourite color?" },
      },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "ask_user no_context reply");

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 11);
  assert.equal(last.result.isError, true);
  assert.match(last.result.content[0].text, /no Lark conversation/);

  await server.shutdown();
});

test("LarkMcpServer: feishu_ask_user routes prompt + auraUserId to askUser handler", async () => {
  let captured = null;
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({ kind: "none" }),
    askUser: async (input, prompt, timeoutMs) => {
      captured = { input, prompt, timeoutMs };
      return { kind: "ok", text: "blue" };
    },
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-ask-ok",
    encodeJson({
      jsonrpc: "2.0",
      id: 12,
      method: "tools/call",
      params: {
        name: "feishu_ask_user",
        arguments: {
          prompt: "what's your favourite color?",
          timeout_seconds: 60,
        },
        _meta: {
          auraUserId: "lark_cli_a_oc_demo_ou_alice",
          auraBotId: "cli_a",
          auraSessionId: "sess-1",
        },
      },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "ask_user reply");

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 12);
  assert.equal(last.result.isError, undefined);
  assert.equal(last.result.content[0].text, "blue");
  assert.deepEqual(captured.input, {
    auraBotId: "cli_a",
    auraSessionId: "sess-1",
    auraUserId: "lark_cli_a_oc_demo_ou_alice",
  });
  assert.equal(captured.prompt, "what's your favourite color?");
  assert.equal(captured.timeoutMs, 60_000);

  await server.shutdown();
});

test("LarkMcpServer: feishu_ask_user rejects timeout_seconds above the agent-contract cap", async () => {
  // Codex regression: the agent's `SIDECAR_MCP_TIMEOUT` is 660s.
  // If `feishu_ask_user` accepted timeouts above ~600s, the agent
  // could give up before the sidecar's own timer fires, leaving an
  // orphan waiter that silently consumes the user's late reply.
  // The schema cap is 600s; anything above it must be rejected at
  // tool-call validation, not silently clamped.
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({ kind: "none" }),
    askUser: async () => ({ kind: "ok", text: "should not run" }),
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-ask-overcap",
    encodeJson({
      jsonrpc: "2.0",
      id: 14,
      method: "tools/call",
      params: {
        name: "feishu_ask_user",
        arguments: { prompt: "wait forever", timeout_seconds: 700 },
      },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "ask_user over-cap reply");

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 14);
  assert.equal(last.result.isError, true);
  // The MCP SDK surfaces zod validation errors as a tool error
  // result. The exact phrasing isn't load-bearing; we just need
  // confirmation the call didn't reach the askUser handler.
  assert.match(
    last.result.content[0].text,
    /timeout_seconds|input|invalid|600/i,
    `expected validation error mentioning timeout/cap, got: ${last.result.content[0].text}`,
  );

  await server.shutdown();
});

test("LarkMcpServer: feishu_ask_user timeout surfaces structured tool error", async () => {
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({ kind: "none" }),
    askUser: async () => ({ kind: "timeout" }),
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-ask-timeout",
    encodeJson({
      jsonrpc: "2.0",
      id: 13,
      method: "tools/call",
      params: {
        name: "feishu_ask_user",
        arguments: { prompt: "still there?", timeout_seconds: 30 },
      },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "ask_user timeout reply");

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 13);
  assert.equal(last.result.isError, true);
  assert.match(last.result.content[0].text, /timed out after 30s/);

  await server.shutdown();
});

test("LarkMcpServer: feishu_list_chat_members routes through rawClient with the resolved chat", async () => {
  let receivedPayload = null;
  const channel = stubChannel(
    async () => ({}),
    {
      async chatMembersGet(payload) {
        receivedPayload = payload;
        return {
          code: 0,
          msg: "ok",
          data: {
            items: [
              { member_id_type: "open_id", member_id: "ou_a", name: "Alice" },
              { member_id_type: "open_id", member_id: "ou_b", name: "Bob" },
            ],
            has_more: false,
            member_total: 2,
          },
        };
      },
    },
  );
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({
      kind: "ok",
      channel,
      chatId: "oc_resolved",
      platformUserId: "ou_alice",
    }),
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-members",
    encodeJson({
      jsonrpc: "2.0",
      id: 21,
      method: "tools/call",
      params: {
        name: "feishu_list_chat_members",
        // The LLM hallucinates an unrelated chat id — the tool must
        // ignore it and bind to the resolver's `oc_resolved`.
        arguments: { chat_id: "oc_other", page_size: 50 },
      },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "list_chat_members reply");

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 21);
  assert.equal(last.result.isError, undefined);
  // Tool ignored the LLM-supplied chat id — bound to the resolved chat.
  assert.equal(receivedPayload.path.chat_id, "oc_resolved");
  assert.equal(receivedPayload.params.member_id_type, "open_id");
  // page_size came through; page_token absent stayed absent (omitted).
  assert.equal(receivedPayload.params.page_size, 50);
  assert.equal("page_token" in receivedPayload.params, false);

  const parsed = JSON.parse(last.result.content[0].text);
  assert.equal(parsed.member_total, 2);
  assert.equal(parsed.items[0].name, "Alice");

  await server.shutdown();
});

test("LarkMcpServer: feishu_list_chat_members surfaces non-zero Feishu API codes as tool errors", async () => {
  const channel = stubChannel(
    async () => ({}),
    {
      async chatMembersGet() {
        return { code: 230002, msg: "bot is not in the chat", data: {} };
      },
    },
  );
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({
      kind: "ok",
      channel,
      chatId: "oc_no_bot",
      platformUserId: "ou_alice",
    }),
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-members-err",
    encodeJson({
      jsonrpc: "2.0",
      id: 22,
      method: "tools/call",
      params: { name: "feishu_list_chat_members", arguments: {} },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "list_chat_members err reply");

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 22);
  assert.equal(last.result.isError, true);
  assert.match(
    last.result.content[0].text,
    /Feishu API error 230002.*bot is not in the chat/,
  );

  await server.shutdown();
});

test("LarkMcpServer: feishu_get_chat_history binds container_id to the resolved chat", async () => {
  let receivedPayload = null;
  const channel = stubChannel(
    async () => ({}),
    {
      async messageList(payload) {
        receivedPayload = payload;
        return {
          code: 0,
          data: {
            has_more: true,
            page_token: "pt_next",
            items: [
              {
                message_id: "om_1",
                msg_type: "text",
                body: { content: "{\"text\":\"hi\"}" },
                sender: {
                  id: "ou_alice",
                  id_type: "open_id",
                  sender_type: "user",
                },
                create_time: "1700000000000",
              },
            ],
          },
        };
      },
    },
  );
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({
      kind: "ok",
      channel,
      chatId: "oc_history",
      platformUserId: "ou_alice",
    }),
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-history",
    encodeJson({
      jsonrpc: "2.0",
      id: 23,
      method: "tools/call",
      params: {
        name: "feishu_get_chat_history",
        arguments: {
          // LLM tries to override container — must be ignored.
          container_id: "oc_other",
          limit: 5,
          sort_type: "ByCreateTimeAsc",
        },
      },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "get_chat_history reply");

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 23);
  assert.equal(last.result.isError, undefined);
  // Bound to resolver's chat, not the LLM's `container_id` arg.
  assert.equal(receivedPayload.params.container_id, "oc_history");
  assert.equal(receivedPayload.params.container_id_type, "chat");
  assert.equal(receivedPayload.params.page_size, 5);
  assert.equal(receivedPayload.params.sort_type, "ByCreateTimeAsc");

  const parsed = JSON.parse(last.result.content[0].text);
  assert.equal(parsed.page_token, "pt_next");
  assert.equal(parsed.items[0].message_id, "om_1");

  await server.shutdown();
});

test("LarkMcpServer: feishu_search_chats forwards query + pagination to chat.search", async () => {
  let receivedPayload = null;
  const channel = stubChannel(
    async () => ({}),
    {
      async chatSearch(payload) {
        receivedPayload = payload;
        return {
          code: 0,
          data: {
            items: [
              { chat_id: "oc_one", name: "Engineering", chat_status: "normal" },
              { chat_id: "oc_two", name: "Engineering Alumni", chat_status: "normal" },
            ],
            has_more: true,
            page_token: "pt_next",
          },
        };
      },
    },
  );
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({
      kind: "ok",
      channel,
      chatId: "oc_active",
      platformUserId: "ou_alice",
    }),
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-search-chats",
    encodeJson({
      jsonrpc: "2.0",
      id: 31,
      method: "tools/call",
      params: {
        name: "feishu_search_chats",
        arguments: { query: "engineering", page_size: 50 },
      },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "search_chats reply");

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 31);
  assert.equal(last.result.isError, undefined);
  assert.equal(receivedPayload.params.query, "engineering");
  assert.equal(receivedPayload.params.page_size, 50);
  assert.equal("page_token" in receivedPayload.params, false);
  const parsed = JSON.parse(last.result.content[0].text);
  assert.equal(parsed.items.length, 2);
  assert.equal(parsed.page_token, "pt_next");

  await server.shutdown();
});

test("LarkMcpServer: feishu_get_message returns the message when chat_id matches the active chat", async () => {
  let receivedPayload = null;
  const channel = stubChannel(
    async () => ({}),
    {
      async messageGet(payload) {
        receivedPayload = payload;
        return {
          code: 0,
          data: {
            items: [
              {
                message_id: "om_target",
                chat_id: "oc_active",
                msg_type: "text",
                body: { content: "{\"text\":\"hello\"}" },
                sender: {
                  id: "ou_alice",
                  id_type: "open_id",
                  sender_type: "user",
                },
                create_time: "1700000001000",
              },
            ],
          },
        };
      },
    },
  );
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({
      kind: "ok",
      channel,
      chatId: "oc_active",
      platformUserId: "ou_alice",
    }),
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-msg-ok",
    encodeJson({
      jsonrpc: "2.0",
      id: 32,
      method: "tools/call",
      params: {
        name: "feishu_get_message",
        arguments: { message_id: "om_target" },
      },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "get_message ok reply");

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 32);
  assert.equal(last.result.isError, undefined);
  assert.equal(receivedPayload.path.message_id, "om_target");
  const parsed = JSON.parse(last.result.content[0].text);
  assert.equal(parsed.message_id, "om_target");
  assert.equal(parsed.chat_id, "oc_active");

  await server.shutdown();
});

test("LarkMcpServer: feishu_get_message refuses cross-chat lookups", async () => {
  // Even though the bot can technically read messages from any chat
  // it belongs to, the tool must refuse to return a message whose
  // chat_id != the resolver's chat — otherwise a paired user could
  // coax the agent into leaking content from sibling chats. Same
  // invariant as feishu_get_chat_info post-Codex #2.
  const channel = stubChannel(
    async () => ({}),
    {
      async messageGet() {
        return {
          code: 0,
          data: {
            items: [
              {
                message_id: "om_other_chat",
                chat_id: "oc_secret",
                msg_type: "text",
                body: { content: "{\"text\":\"private\"}" },
              },
            ],
          },
        };
      },
    },
  );
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({
      kind: "ok",
      channel,
      chatId: "oc_active",
      platformUserId: "ou_alice",
    }),
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-msg-cross",
    encodeJson({
      jsonrpc: "2.0",
      id: 33,
      method: "tools/call",
      params: {
        name: "feishu_get_message",
        arguments: { message_id: "om_other_chat" },
      },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "get_message cross-chat reply");

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 33);
  assert.equal(last.result.isError, true);
  assert.match(
    last.result.content[0].text,
    /belongs to a different chat than the active conversation/,
  );
  assert.doesNotMatch(last.result.content[0].text, /private/);

  await server.shutdown();
});

test("LarkMcpServer: feishu_get_message reports not-found cleanly when API returns no items", async () => {
  const channel = stubChannel(
    async () => ({}),
    {
      async messageGet() {
        return { code: 0, data: { items: [] } };
      },
    },
  );
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({
      kind: "ok",
      channel,
      chatId: "oc_active",
      platformUserId: "ou_alice",
    }),
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-msg-empty",
    encodeJson({
      jsonrpc: "2.0",
      id: 34,
      method: "tools/call",
      params: {
        name: "feishu_get_message",
        arguments: { message_id: "om_missing" },
      },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "get_message not-found reply");

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 34);
  assert.equal(last.result.isError, true);
  assert.match(last.result.content[0].text, /no message found for id om_missing/);

  await server.shutdown();
});

test("LarkMcpServer: feishu_who_am_i returns toolError when UAT pipeline isn't wired", async () => {
  // The accessor is optional on LarkChannelResolution.ok — a bot
  // without the `secrets` capability negotiated still responds
  // cleanly instead of throwing inside the tool.
  const channel = stubChannel(async () => ({}), {});
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({
      kind: "ok",
      channel,
      chatId: "oc_active",
      platformUserId: "ou_alice",
      // uatAccessor intentionally omitted.
    }),
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-no-uat",
    encodeJson({
      jsonrpc: "2.0",
      id: 41,
      method: "tools/call",
      params: { name: "feishu_who_am_i", arguments: {} },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "no-uat reply");

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 41);
  assert.equal(last.result.isError, true);
  assert.match(last.result.content[0].text, /UAT.*pipeline isn't configured/);
});

test("LarkMcpServer: feishu_who_am_i routes through UATAccessor with platformUserId", async () => {
  let receivedRequest = null;
  const fakeAccessor = {
    async invoke(req, handler) {
      receivedRequest = req;
      // Pretend we already have a UAT — call the handler directly
      // and return its result wrapped in `ok`.
      const result = await handler("uat_for_alice");
      return { kind: "ok", result };
    },
  };
  let receivedAuthOpts = null;
  const channel = stubChannel(async () => ({}), {
    async authenUserInfoGet(_payload, opts) {
      receivedAuthOpts = opts;
      return {
        code: 0,
        data: {
          name: "Alice",
          en_name: "Alice",
          open_id: "ou_alice",
          email: "alice@example.com",
        },
      };
    },
  });
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({
      kind: "ok",
      channel,
      chatId: "oc_active",
      platformUserId: "ou_alice",
      uatAccessor: fakeAccessor,
    }),
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-who",
    encodeJson({
      jsonrpc: "2.0",
      id: 42,
      method: "tools/call",
      params: { name: "feishu_who_am_i", arguments: {} },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "who_am_i reply");

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 42);
  assert.equal(last.result.isError, undefined);

  // Accessor was called with the resolver's platformUserId — proves
  // the LLM can't smuggle a different user id since the tool's
  // inputSchema is empty.
  assert.equal(receivedRequest.userOpenId, "ou_alice");
  assert.equal(receivedRequest.chatId, "oc_active");
  assert.match(receivedRequest.reason, /Feishu profile/);

  // The handler called channel.rawClient.authen.userInfo.get with
  // the user-access-token option set by lark.withUserAccessToken.
  assert.ok(receivedAuthOpts);
  // The opts shape is the SDK's IRequestOption — opaque to us, but
  // it MUST be present (without it the call would use tenant token).
  // Sanity: at minimum the opts is an object.
  assert.equal(typeof receivedAuthOpts, "object");

  const parsed = JSON.parse(last.result.content[0].text);
  assert.equal(parsed.name, "Alice");
  assert.equal(parsed.open_id, "ou_alice");
});

test("LarkMcpServer: feishu_who_am_i surfaces auth-failed outcomes as readable tool errors", async () => {
  const fakeAccessor = {
    async invoke() {
      return {
        kind: "auth_failed",
        outcome: { kind: "denied" },
      };
    },
  };
  const channel = stubChannel(async () => ({}), {});
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({
      kind: "ok",
      channel,
      chatId: "oc_x",
      platformUserId: "ou_alice",
      uatAccessor: fakeAccessor,
    }),
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-denied",
    encodeJson({
      jsonrpc: "2.0",
      id: 43,
      method: "tools/call",
      params: { name: "feishu_who_am_i", arguments: {} },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "denied reply");

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.result.isError, true);
  assert.match(last.result.content[0].text, /declined the authorization/);
});

test("LarkMcpServer: feishu_get_user routes through contact.v3.user.get with UAT", async () => {
  let receivedPayload = null;
  let receivedOpts = null;
  const fakeAccessor = {
    async invoke(_req, handler) {
      const result = await handler("uat_for_alice");
      return { kind: "ok", result };
    },
  };
  const channel = stubChannel(async () => ({}), {
    async contactUserGet(payload, opts) {
      receivedPayload = payload;
      receivedOpts = opts;
      return {
        code: 0,
        data: {
          user: {
            open_id: "ou_bob",
            name: "Bob",
            email: "bob@example.com",
          },
        },
      };
    },
  });
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({
      kind: "ok",
      channel,
      chatId: "oc_active",
      platformUserId: "ou_alice",
      uatAccessor: fakeAccessor,
    }),
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-get-user",
    encodeJson({
      jsonrpc: "2.0",
      id: 51,
      method: "tools/call",
      params: {
        name: "feishu_get_user",
        arguments: { user_id: "ou_bob", user_id_type: "open_id" },
      },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "get_user reply");

  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.id, 51);
  assert.equal(last.result.isError, undefined);
  assert.equal(receivedPayload.path.user_id, "ou_bob");
  assert.equal(receivedPayload.params.user_id_type, "open_id");
  // UAT was attached.
  assert.ok(receivedOpts);
  const parsed = JSON.parse(last.result.content[0].text);
  assert.equal(parsed.user.name, "Bob");
});

test("LarkMcpServer: feishu_get_user defaults user_id_type to open_id", async () => {
  let receivedParams = null;
  const fakeAccessor = {
    async invoke(_req, handler) {
      return { kind: "ok", result: await handler("uat") };
    },
  };
  const channel = stubChannel(async () => ({}), {
    async contactUserGet(payload) {
      receivedParams = payload.params;
      return { code: 0, data: { user: {} } };
    },
  });
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({
      kind: "ok",
      channel,
      chatId: "oc_x",
      platformUserId: "ou_alice",
      uatAccessor: fakeAccessor,
    }),
  });
  const reply = captureReply();
  await initialize(server, reply);

  await server.accept(
    "tunnel-get-user-default",
    encodeJson({
      jsonrpc: "2.0",
      id: 52,
      method: "tools/call",
      params: {
        name: "feishu_get_user",
        // No user_id_type — must default to open_id, not flow as
        // undefined into the API call (Feishu rejects undefined).
        arguments: { user_id: "ou_x" },
      },
    }),
    reply.handle,
  );
  await waitFor(() => receivedParams !== null, "get_user default reply");
  assert.equal(receivedParams.user_id_type, "open_id");
});

test("LarkMcpServer: feishu_search_user calls /open-apis/search/v1/user with bearer UAT", async () => {
  // Stub global fetch since feishu_search_user goes through raw
  // fetch (the SDK doesn't expose this endpoint typed).
  const originalFetch = globalThis.fetch;
  const fetchCalls = [];
  globalThis.fetch = async (url, init) => {
    fetchCalls.push({ url, init });
    return {
      ok: true,
      status: 200,
      async json() {
        return {
          code: 0,
          data: {
            users: [{ open_id: "ou_match", name: "Carol" }],
            has_more: false,
          },
        };
      },
    };
  };
  try {
    const fakeAccessor = {
      baseUrl: "https://open.feishu.cn",
      async invoke(_req, handler) {
        const result = await handler("uat_for_alice");
        return { kind: "ok", result };
      },
    };
    const channel = stubChannel(async () => ({}), {});
    const server = new LarkMcpServer({
      logger: noopLogger,
      channelResolver: () => ({
        kind: "ok",
        channel,
        chatId: "oc_active",
        platformUserId: "ou_alice",
        uatAccessor: fakeAccessor,
      }),
    });
    const reply = captureReply();
    await initialize(server, reply);

    const before = reply.sent.length;
    await server.accept(
      "tunnel-search-user",
      encodeJson({
        jsonrpc: "2.0",
        id: 53,
        method: "tools/call",
        params: {
          name: "feishu_search_user",
          arguments: { query: "Carol", page_size: 5 },
        },
      }),
      reply.handle,
    );
    await waitFor(() => reply.sent.length > before, "search_user reply");

    assert.equal(fetchCalls.length, 1);
    const url = fetchCalls[0].url;
    assert.match(
      url,
      /^https:\/\/open\.feishu\.cn\/open-apis\/search\/v1\/user\?/,
    );
    assert.match(url, /query=Carol/);
    assert.match(url, /page_size=5/);
    assert.equal(
      fetchCalls[0].init.headers.Authorization,
      "Bearer uat_for_alice",
    );

    const last = decodeJson(reply.sent[reply.sent.length - 1]);
    assert.equal(last.id, 53);
    assert.equal(last.result.isError, undefined);
    const parsed = JSON.parse(last.result.content[0].text);
    assert.equal(parsed.users[0].name, "Carol");
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test("LarkMcpServer: feishu_calendar dispatches to primary/list/get based on action", async () => {
  const calls = { primary: 0, list: null, get: null };
  const fakeAccessor = {
    async invoke(_req, handler) {
      return { kind: "ok", result: await handler("uat") };
    },
  };
  const channel = stubChannel(async () => ({}), {
    async calendarPrimary() {
      calls.primary++;
      return { code: 0, data: { calendars: [{ calendar: { calendar_id: "cal_p" } }] } };
    },
    async calendarList(payload) {
      calls.list = payload;
      return { code: 0, data: { calendar_list: [], has_more: false } };
    },
    async calendarGet(payload) {
      calls.get = payload;
      return { code: 0, data: { calendar_id: payload.path.calendar_id } };
    },
  });
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({
      kind: "ok",
      channel,
      chatId: "oc_x",
      platformUserId: "ou_alice",
      uatAccessor: fakeAccessor,
    }),
  });
  const reply = captureReply();
  await initialize(server, reply);

  async function call(id, args) {
    const before = reply.sent.length;
    await server.accept(
      `tunnel-cal-${id}`,
      encodeJson({
        jsonrpc: "2.0",
        id,
        method: "tools/call",
        params: { name: "feishu_calendar", arguments: args },
      }),
      reply.handle,
    );
    await waitFor(() => reply.sent.length > before, `calendar id=${id}`);
    return decodeJson(reply.sent[reply.sent.length - 1]);
  }

  await call(60, { action: "primary" });
  await call(61, { action: "list", page_size: 100, page_token: "pt_x" });
  await call(62, { action: "get", calendar_id: "cal_target" });
  const badGet = await call(63, { action: "get" });

  assert.equal(calls.primary, 1);
  assert.equal(calls.list.params.page_size, 100);
  assert.equal(calls.list.params.page_token, "pt_x");
  assert.equal(calls.get.path.calendar_id, "cal_target");

  assert.equal(badGet.id, 63);
  assert.equal(badGet.result.isError, true);
  assert.match(badGet.result.content[0].text, /requires `calendar_id`/);
});

test("LarkMcpServer: feishu_freebusy rejects mutually-exclusive user_id + room_id", async () => {
  const fakeAccessor = {
    async invoke(_req, handler) {
      return { kind: "ok", result: await handler("uat") };
    },
  };
  const channel = stubChannel(async () => ({}), {});
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({
      kind: "ok",
      channel,
      chatId: "oc_x",
      platformUserId: "ou_alice",
      uatAccessor: fakeAccessor,
    }),
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-fb-bad",
    encodeJson({
      jsonrpc: "2.0",
      id: 64,
      method: "tools/call",
      params: {
        name: "feishu_freebusy",
        arguments: {
          time_min: "2026-05-02T09:00:00+08:00",
          time_max: "2026-05-02T18:00:00+08:00",
          user_id: "ou_x",
          room_id: "room_y",
        },
      },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "freebusy bad reply");
  const last = decodeJson(reply.sent[reply.sent.length - 1]);
  assert.equal(last.result.isError, true);
  assert.match(last.result.content[0].text, /mutually exclusive/);
});

test("LarkMcpServer: feishu_freebusy_batch forwards user_ids list to calendar.freebusy.batch", async () => {
  let receivedPayload = null;
  const fakeAccessor = {
    async invoke(_req, handler) {
      return { kind: "ok", result: await handler("uat") };
    },
  };
  const channel = stubChannel(async () => ({}), {
    async freebusyBatch(payload) {
      receivedPayload = payload;
      return {
        code: 0,
        data: {
          freebusy_lists: [{ user_id: "ou_x", freebusy_items: [] }],
        },
      };
    },
  });
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({
      kind: "ok",
      channel,
      chatId: "oc_x",
      platformUserId: "ou_alice",
      uatAccessor: fakeAccessor,
    }),
  });
  const reply = captureReply();
  await initialize(server, reply);

  const before = reply.sent.length;
  await server.accept(
    "tunnel-fb-batch",
    encodeJson({
      jsonrpc: "2.0",
      id: 65,
      method: "tools/call",
      params: {
        name: "feishu_freebusy_batch",
        arguments: {
          time_min: "2026-05-02T09:00:00+08:00",
          time_max: "2026-05-02T18:00:00+08:00",
          user_ids: ["ou_x", "ou_y"],
        },
      },
    }),
    reply.handle,
  );
  await waitFor(() => reply.sent.length > before, "freebusy batch reply");

  assert.deepEqual(receivedPayload.data.user_ids, ["ou_x", "ou_y"]);
  assert.equal(receivedPayload.params.user_id_type, "open_id");
});

test("LarkMcpServer: distinct tunnel_ids get independent server sessions", async () => {
  const channel = stubChannel(async (chatId) => ({ chatId, name: "X" }));
  const server = new LarkMcpServer({
    logger: noopLogger,
    channelResolver: () => ({
      kind: "ok",
      channel,
      chatId: "oc_x",
      platformUserId: "ou_alice",
    }),
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
