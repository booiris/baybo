import test from "node:test";
import assert from "node:assert/strict";

import {
  buildResolvedWriteApprovalCard,
  buildWriteApprovalCard,
  parseWriteApprovalCardValue,
} from "../dist/messaging/write-approval.js";

test("buildWriteApprovalCard renders header + summary + Approve/Deny buttons", () => {
  const card = buildWriteApprovalCard({
    callId: "mcpw_x",
    toolName: "feishu_calendar_event_create",
    summary: 'Create event "Q3 review" on 2026-05-02 14:00',
  });
  assert.equal(card.header.template, "orange");
  assert.match(card.header.title.content, /Approve write/);
  const action = card.elements.find((e) => e.tag === "action");
  assert.equal(action.actions.length, 2);
  assert.equal(action.actions[0].text.content, "Approve");
  assert.equal(action.actions[0].type, "primary");
  assert.equal(action.actions[1].text.content, "Deny");
  assert.equal(action.actions[1].type, "danger");
  // Both buttons carry the same call_id + the mcp_write tag so the
  // platform's router can distinguish them from bash approvals.
  assert.deepEqual(action.actions[0].value, {
    aura: "mcp_write",
    call_id: "mcpw_x",
    decision: "approve",
  });
  assert.deepEqual(action.actions[1].value, {
    aura: "mcp_write",
    call_id: "mcpw_x",
    decision: "deny",
  });
});

test("buildWriteApprovalCard includes optional detail block when present", () => {
  const card = buildWriteApprovalCard({
    callId: "x",
    toolName: "feishu_bitable_records_create",
    summary: "Insert 5 rows",
    detail: "Row 1: foo=bar\nRow 2: ...",
  });
  // Two div elements + one action element when detail set.
  const divs = card.elements.filter((e) => e.tag === "div");
  assert.equal(divs.length, 2);
  assert.match(divs[1].text.content, /Row 1: foo=bar/);
});

test("buildWriteApprovalCard truncates very long detail to keep card sane", () => {
  const card = buildWriteApprovalCard({
    callId: "x",
    toolName: "t",
    summary: "s",
    detail: "x".repeat(5000),
  });
  const detailDiv = card.elements.filter((e) => e.tag === "div")[1];
  // Cap at ~1500 chars + the fence.
  assert.ok(detailDiv.text.content.length < 1700);
  assert.match(detailDiv.text.content, /^```\n/);
});

test("buildResolvedWriteApprovalCard turns green/red on decision", () => {
  const opts = { callId: "x", toolName: "t", summary: "s" };
  const approved = buildResolvedWriteApprovalCard(opts, "approve");
  assert.equal(approved.header.template, "green");
  assert.match(approved.header.title.content, /Approved/);
  assert.match(approved.elements[0].text.content, /Proceeding/);

  const denied = buildResolvedWriteApprovalCard(opts, "deny");
  assert.equal(denied.header.template, "red");
  assert.match(denied.header.title.content, /Denied/);
  assert.match(denied.elements[0].text.content, /will not proceed/);
});

test("parseWriteApprovalCardValue accepts mcp_write-tagged values", () => {
  assert.deepEqual(
    parseWriteApprovalCardValue({
      aura: "mcp_write",
      call_id: "mcpw_x",
      decision: "approve",
    }),
    { callId: "mcpw_x", decision: "approve" },
  );
  assert.deepEqual(
    parseWriteApprovalCardValue({
      aura: "mcp_write",
      call_id: "mcpw_y",
      decision: "deny",
    }),
    { callId: "mcpw_y", decision: "deny" },
  );
});

test("parseWriteApprovalCardValue rejects bash-approval (aura: 'approval') values", () => {
  // Sanity: the router picks per-tag so an MCP-write parser must
  // reject a bash-approval click — otherwise the wrong handler
  // would resolve the wrong waiter.
  assert.equal(
    parseWriteApprovalCardValue({
      aura: "approval",
      call_id: "bash_call",
      decision: "approve",
    }),
    null,
  );
});

test("parseWriteApprovalCardValue rejects malformed shapes", () => {
  assert.equal(parseWriteApprovalCardValue(null), null);
  assert.equal(parseWriteApprovalCardValue("string"), null);
  assert.equal(parseWriteApprovalCardValue({}), null);
  assert.equal(
    parseWriteApprovalCardValue({ aura: "mcp_write", call_id: 42, decision: "approve" }),
    null,
  );
  assert.equal(
    parseWriteApprovalCardValue({ aura: "mcp_write", call_id: "x", decision: "yes" }),
    null,
  );
});
