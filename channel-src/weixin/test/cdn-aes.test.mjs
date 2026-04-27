// AES-128-ECB primitive round-trip + padded-size assertions. The
// iLink CDN is the only place we ever use ECB, so the failure mode
// for a bad implementation here is "every media upload silently
// produces garbage" — worth a tight sanity test.

import test from "node:test";
import assert from "node:assert/strict";

import {
  aesEcbPaddedSize,
  decryptAesEcb,
  encryptAesEcb,
} from "../dist/cdn/aes-ecb.js";

const KEY = Buffer.from("0123456789abcdef0123456789abcdef", "hex");

test("encrypt + decrypt roundtrips arbitrary payload", () => {
  const plain = Buffer.from("hello iLink CDN — 你好", "utf-8");
  const cipher = encryptAesEcb(plain, KEY);
  const back = decryptAesEcb(cipher, KEY);
  assert.deepEqual(back, plain);
});

test("aesEcbPaddedSize matches what AES-128-ECB / PKCS7 actually emits", () => {
  for (const size of [0, 1, 15, 16, 17, 31, 32, 33, 4095, 4096]) {
    const plain = Buffer.alloc(size, 0x42);
    const cipher = encryptAesEcb(plain, KEY);
    assert.equal(
      aesEcbPaddedSize(size),
      cipher.length,
      `size=${size} mismatch: predicted=${aesEcbPaddedSize(size)} actual=${cipher.length}`,
    );
  }
});

test("encrypt is deterministic for the same key + plaintext", () => {
  const plain = Buffer.from("repeat-me", "utf-8");
  const a = encryptAesEcb(plain, KEY);
  const b = encryptAesEcb(plain, KEY);
  assert.deepEqual(a, b);
});
