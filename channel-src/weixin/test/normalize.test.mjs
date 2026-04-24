import test from "node:test";
import assert from "node:assert/strict";

import { normalizeBotId } from "../dist/auth/normalize.js";

test("normalizeBotId replaces @ and . with dashes", () => {
  assert.equal(normalizeBotId("b0f5860fdecb@im.bot"), "b0f5860fdecb-im-bot");
});

test("normalizeBotId drops unsupported characters", () => {
  assert.equal(normalizeBotId("abc$:def%ghi!jkl"), "abcdefghijkl");
});

test("normalizeBotId preserves alphanumerics, dash, and underscore", () => {
  assert.equal(normalizeBotId("A1-b2_c3"), "A1-b2_c3");
});

test("normalizeBotId trims leading/trailing whitespace before normalizing", () => {
  assert.equal(normalizeBotId("  xyz@im.bot  "), "xyz-im-bot");
});

test("normalizeBotId caps output at 64 chars", () => {
  const raw = "a".repeat(100);
  assert.equal(normalizeBotId(raw).length, 64);
});

test("normalizeBotId rejects empty input", () => {
  assert.throws(() => normalizeBotId(""));
});

test("normalizeBotId rejects input with no surviving characters", () => {
  // `$%^!` contains no chars allowed by the filter (no A-Z a-z 0-9 _ -)
  // and nothing for `@.` to rewrite to, so the output is empty.
  assert.throws(() => normalizeBotId("$%^!"));
});
