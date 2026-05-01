import test from "node:test";
import assert from "node:assert/strict";

import {
  buildApprovalCard,
  buildResolvedApprovalCard,
  parseApprovalCardValue,
} from "../dist/messaging/cards.js";

const sampleReq = {
  callId: "call-42",
  sessionId: "s",
  userId: "lark_cli_a1_chatId=oc_xxx_ou_yyy",
  tool: "Bash",
  paramsPreview: 'echo "hello"',
  description: "Run a shell command",
};

test("buildApprovalCard: emits three buttons with our own value payload", () => {
  const card = buildApprovalCard(sampleReq);
  const action = card.elements.find((e) => e.tag === "action");
  assert.ok(action, "action element present");
  const decisions = action.actions.map((a) => a.value.decision).sort();
  assert.deepEqual(decisions, ["approve", "approve_always", "deny"]);
  for (const a of action.actions) {
    assert.equal(a.value.aura, "approval");
    assert.equal(a.value.call_id, sampleReq.callId);
  }
});

test("parseApprovalCardValue: round-trips every decision", () => {
  for (const decision of ["approve", "approve_always", "deny"]) {
    const out = parseApprovalCardValue({
      aura: "approval",
      call_id: "c",
      decision,
    });
    assert.deepEqual(out, { callId: "c", decision });
  }
});

test("parseApprovalCardValue: rejects foreign card values", () => {
  assert.equal(
    parseApprovalCardValue({ kind: "feedback", call_id: "x" }),
    null,
  );
  assert.equal(parseApprovalCardValue(null), null);
  assert.equal(parseApprovalCardValue("approve"), null);
  assert.equal(
    parseApprovalCardValue({ aura: "approval", call_id: "x", decision: "weird" }),
    null,
  );
});

test("buildResolvedApprovalCard: decision colors header template", () => {
  const a = buildResolvedApprovalCard(sampleReq, "approve");
  const d = buildResolvedApprovalCard(sampleReq, "deny");
  const aa = buildResolvedApprovalCard(sampleReq, "approve_always");
  assert.equal(a.header.template, "turquoise");
  assert.equal(d.header.template, "red");
  assert.equal(aa.header.template, "green");
});
