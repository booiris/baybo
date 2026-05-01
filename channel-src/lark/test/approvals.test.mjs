import test from "node:test";
import assert from "node:assert/strict";

import { LarkApprovals } from "../dist/approvals.js";

const noopLogger = {
  debug() {},
  info() {},
  warn() {},
  error() {},
};

// Build a stub LarkChannel that records every send / updateCard call.
function stubHandle() {
  const sent = [];
  const updated = [];
  return {
    sent,
    updated,
    async send(chatId, payload) {
      sent.push({ chatId, payload });
      return { messageId: `msg-${sent.length}` };
    },
    async updateCard(messageId, card) {
      updated.push({ messageId, card });
    },
  };
}

const sampleReq = {
  callId: "call-1",
  sessionId: "s",
  // composeAuraUserId default form: <channelType>_<botId>_<chatKey>_<openId>
  userId: "lark_cli_a1_chatId=oc_xxx_ou_alice",
  tool: "Bash",
  paramsPreview: 'echo "hi"',
  description: "Run a shell command",
};

const route = (handle) => ({
  botId: "cli_a1",
  handle,
  chat: { chatId: "oc_xxx" },
});

test("handleCardAction: authorized tap edits card to terminal state", async () => {
  const broker = new LarkApprovals(noopLogger);
  const handle = stubHandle();
  const decision = broker.onRequested(sampleReq, route(handle));

  // Wait for the card send to settle so `pending` is populated.
  await Promise.resolve();
  await Promise.resolve();

  broker.handleCardAction({
    messageId: "msg-1",
    chatId: "oc_xxx",
    operator: { openId: "ou_alice" },
    action: {
      tag: "button",
      value: { aura: "approval", call_id: "call-1", decision: "approve" },
    },
  });

  assert.equal(await decision, "approve");
  // Yield once for the fire-and-forget updateCard to run.
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(handle.updated.length, 1);
  assert.equal(handle.updated[0].messageId, "msg-1");
  // Resolved card colors header by decision; turquoise for `approve`.
  assert.equal(handle.updated[0].card.header.template, "turquoise");
});

test("handleCardAction: unauthorized tap leaves card untouched and pending alive", async () => {
  const broker = new LarkApprovals(noopLogger);
  const handle = stubHandle();
  const decision = broker.onRequested(sampleReq, route(handle));

  await Promise.resolve();
  await Promise.resolve();

  broker.handleCardAction({
    messageId: "msg-1",
    chatId: "oc_xxx",
    operator: { openId: "ou_someone_else" },
    action: {
      tag: "button",
      value: { aura: "approval", call_id: "call-1", decision: "approve" },
    },
  });

  // No update fired — card stays interactive for the real triggerer.
  assert.equal(handle.updated.length, 0);

  // The triggerer can still resolve.
  broker.handleCardAction({
    messageId: "msg-1",
    chatId: "oc_xxx",
    operator: { openId: "ou_alice" },
    action: {
      tag: "button",
      value: { aura: "approval", call_id: "call-1", decision: "deny" },
    },
  });
  assert.equal(await decision, "deny");
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(handle.updated.length, 1);
  assert.equal(handle.updated[0].card.header.template, "red");
});

test("onResolved: gateway-driven resolution still updates card when no tap landed", async () => {
  const broker = new LarkApprovals(noopLogger);
  const handle = stubHandle();
  const pending = broker.onRequested(sampleReq, route(handle));

  await Promise.resolve();
  await Promise.resolve();

  // Simulate the gateway resolving without a user tap (e.g. agent-side
  // timeout or auto-deny). Pending entry is still alive — onResolved
  // owns the terminal edit on this path.
  await broker.onResolved("call-1", "approve_always");
  assert.equal(await pending, "approve_always");
  assert.equal(handle.updated.length, 1);
  assert.equal(handle.updated[0].card.header.template, "green");
});
