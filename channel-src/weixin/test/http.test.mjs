import test from "node:test";
import assert from "node:assert/strict";

import { buildClientVersion } from "../dist/api/http.js";

test("buildClientVersion packs semver into 0x00MMNNPP", () => {
  assert.equal(buildClientVersion("1.0.11"), 0x0001000b);
  assert.equal(buildClientVersion("1.0.11"), 65547);
});

test("buildClientVersion treats missing components as 0", () => {
  assert.equal(buildClientVersion("2"), 0x00020000);
  assert.equal(buildClientVersion("2.3"), 0x00020300);
  assert.equal(buildClientVersion(""), 0);
});

test("buildClientVersion masks overflowing components to u8", () => {
  // 256 & 0xff === 0; the masking keeps the packing predictable.
  assert.equal(buildClientVersion("256.0.0"), 0);
  assert.equal(buildClientVersion("255.255.255"), 0x00ffffff);
});

test("buildClientVersion is stable across equivalent inputs", () => {
  assert.equal(buildClientVersion("0.1.0"), buildClientVersion("0.01.00"));
});
