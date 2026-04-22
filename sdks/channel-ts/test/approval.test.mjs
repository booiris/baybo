import test from "node:test";
import assert from "node:assert/strict";

import { normalizeDecision } from "../dist/runner.js";

test("normalizeDecision accepts all three Rust ApprovalDecision variants", () => {
  assert.equal(normalizeDecision("approve"), "approve");
  assert.equal(normalizeDecision("approve_always"), "approve_always");
  assert.equal(normalizeDecision("deny"), "deny");
});

test("normalizeDecision rejects unknown strings", () => {
  assert.equal(normalizeDecision(""), null);
  assert.equal(normalizeDecision("APPROVE"), null);
  assert.equal(normalizeDecision("allow"), null);
  assert.equal(normalizeDecision("approved"), null);
  assert.equal(normalizeDecision("maybe"), null);
});
