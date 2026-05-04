import test from "node:test";
import assert from "node:assert/strict";

import { UATStore } from "../dist/auth/uat-store.js";

class MemoryScope {
  constructor() {
    this.map = new Map();
  }
  async get(key) {
    return this.map.has(key) ? this.map.get(key) : null;
  }
  async set(key, value) {
    this.map.set(key, value);
  }
  async delete(key) {
    this.map.delete(key);
  }
  async list() {
    return [...this.map.keys()];
  }
}

const SAMPLE_TOKEN = {
  accessToken: "uat_v2_xxx",
  refreshToken: "ur_v2_yyy",
  expiresIn: 7200,
  refreshExpiresIn: 604800,
  scope: "im:message offline_access",
};

test("UATStore round-trips a fresh device-flow grant", async () => {
  const scope = new MemoryScope();
  const store = new UATStore(scope);
  const before = Date.now();
  const stored = await store.storeGrant("ou_alice", SAMPLE_TOKEN);
  const after = Date.now();

  assert.equal(stored.accessToken, "uat_v2_xxx");
  assert.equal(stored.refreshToken, "ur_v2_yyy");
  assert.equal(stored.scope, "im:message offline_access");
  assert.ok(stored.grantedAt >= before);
  assert.ok(stored.grantedAt <= after);
  // expiresAt = now + 7200s, ±200ms wall clock slack
  assert.ok(stored.expiresAt >= before + 7200_000);
  assert.ok(stored.expiresAt <= after + 7200_000);

  const got = await store.get("ou_alice");
  assert.deepEqual(got, stored);
});

test("UATStore.storeGrant accepts an explicit grantedAt to preserve across refresh", async () => {
  const scope = new MemoryScope();
  const store = new UATStore(scope);
  // Original grant 24h ago — typical refresh path: token rotated but
  // we want to remember WHEN the user first authorised.
  const originalGrantMs = Date.now() - 86_400_000;
  const stored = await store.storeGrant(
    "ou_alice",
    SAMPLE_TOKEN,
    originalGrantMs,
  );
  assert.equal(stored.grantedAt, originalGrantMs);
});

test("UATStore.get returns null for unknown user", async () => {
  const store = new UATStore(new MemoryScope());
  assert.equal(await store.get("ou_unknown"), null);
});

test("UATStore.get returns null on corrupted JSON instead of throwing", async () => {
  // Anti-DoS: a corrupted vault entry shouldn't poison every tool call
  // for that user — surface as "unauthorised" so the auth flow re-runs.
  const scope = new MemoryScope();
  await scope.set("uat/ou_alice", "{not valid json");
  const store = new UATStore(scope);
  assert.equal(await store.get("ou_alice"), null);
});

test("UATStore.get returns null on schema-mismatched JSON", async () => {
  // Forward-compat: if a future StoredUAToken adds required fields, an
  // old entry should look "unauthorised" and trigger re-auth, not crash.
  const scope = new MemoryScope();
  await scope.set("uat/ou_alice", JSON.stringify({ wrongShape: true }));
  const store = new UATStore(scope);
  assert.equal(await store.get("ou_alice"), null);
});

test("UATStore.delete removes the entry", async () => {
  const scope = new MemoryScope();
  const store = new UATStore(scope);
  await store.storeGrant("ou_alice", SAMPLE_TOKEN);
  await store.delete("ou_alice");
  assert.equal(await store.get("ou_alice"), null);
});

test("UATStore.listUsers strips the prefix and ignores non-UAT keys", async () => {
  const scope = new MemoryScope();
  // Mix in a non-UAT key — the bot may store other secrets in the
  // same scope (e.g. an unrelated subsystem's encryption key).
  await scope.set("encryption_key", "redacted");
  const store = new UATStore(scope);
  await store.storeGrant("ou_alice", SAMPLE_TOKEN);
  await store.storeGrant("ou_bob", SAMPLE_TOKEN);

  const users = (await store.listUsers()).sort();
  assert.deepEqual(users, ["ou_alice", "ou_bob"]);
});

test("UATStore namespaces keys under uat/ to avoid colliding with other secrets", async () => {
  const scope = new MemoryScope();
  const store = new UATStore(scope);
  await store.storeGrant("ou_alice", SAMPLE_TOKEN);
  // The raw key in the underlying scope MUST be prefixed.
  assert.deepEqual([...scope.map.keys()], ["uat/ou_alice"]);
});

test("UATStore rejects empty userOpenId so we never blow away the entire scope", async () => {
  const store = new UATStore(new MemoryScope());
  await assert.rejects(
    store.storeGrant("", SAMPLE_TOKEN),
    /userOpenId must be non-empty/,
  );
});
