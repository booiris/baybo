/**
 * Unit coverage for the StatusCondenser: the in-place status-message
 * state machine, the coalesced single-flight throttle, heartbeat
 * liveness + skip-when-identical, approval pause, terminal finalize,
 * rate-limit backoff, and queued-turn handling. Drives a fake sink with
 * real (short) timers, matching the BotChannel typing tests' style.
 */
import test from "node:test";
import assert from "node:assert/strict";

import { StatusCondenser, StatusRateLimited, fmtElapsed } from "../dist/status.js";

function stubLogger() {
  return { debug() {}, info() {}, warn() {}, error() {} };
}

const delay = (ms) => new Promise((r) => setTimeout(r, ms));

function makeSink() {
  const sends = [];
  const edits = [];
  let nextId = 1;
  let onEdit = null;
  return {
    sends,
    edits,
    last: () => edits[edits.length - 1]?.text,
    setOnEdit: (fn) => { onEdit = fn; },
    sink: {
      async send(userId, text) {
        sends.push({ userId, text });
        return nextId++;
      },
      async edit(userId, messageId, text) {
        edits.push({ userId, messageId, text });
        if (onEdit) onEdit(text);
      },
    },
  };
}

function condenser(fake, options, safetyMs = 100_000) {
  return new StatusCondenser({
    sink: fake.sink,
    logger: stubLogger(),
    options,
    safetyMs,
  });
}

const U = "test_b1_42_u";

test("fmtElapsed: seconds under a minute, m+padded-s over", () => {
  assert.equal(fmtElapsed(0), "0s");
  assert.equal(fmtElapsed(8_000), "8s");
  assert.equal(fmtElapsed(41_900), "41s");
  assert.equal(fmtElapsed(124_000), "2m04s");
  assert.equal(fmtElapsed(132_000), "2m12s");
});

test("a short turn that replies before any tool/appear shows no bubble", async () => {
  const fake = makeSink();
  const c = condenser(fake, { appearAfterMs: 1000 });
  c.onInbound(U);
  c.onReply(U);
  await delay(20);
  assert.equal(fake.sends.length, 0, "no status message for a short turn");
  assert.equal(fake.edits.length, 0);
});

test("the first tool call materializes the bubble immediately", async () => {
  const fake = makeSink();
  const c = condenser(fake, { appearAfterMs: 10_000 });
  c.onInbound(U);
  c.onProgress(U, { kind: "tool", tool: "bash", label: "bash — cargo test" });
  await delay(10);
  assert.equal(fake.sends.length, 1);
  assert.match(fake.sends[0].text, /🔧 bash — cargo test/);
});

test("with no tool, the appear timer materializes a thinking bubble", async () => {
  const fake = makeSink();
  const c = condenser(fake, { appearAfterMs: 30, heartbeatMs: 10_000 });
  c.onInbound(U);
  await delay(10);
  assert.equal(fake.sends.length, 0, "nothing before the appear threshold");
  await delay(40);
  assert.equal(fake.sends.length, 1);
  assert.match(fake.sends[0].text, /🤔 working/);
});

test("bursty progress coalesces to one throttled edit showing the latest", async () => {
  const fake = makeSink();
  const c = condenser(fake, { appearAfterMs: 10_000, minEditIntervalMs: 50, heartbeatMs: 10_000 });
  c.onInbound(U);
  c.onProgress(U, { kind: "tool", tool: "a" }); // materializes (send)
  await delay(5);
  for (const t of ["b", "c", "d", "e"]) c.onProgress(U, { kind: "tool", tool: t });
  await delay(10);
  assert.equal(fake.edits.length, 0, "edits are throttled within the min interval");
  await delay(70);
  assert.equal(fake.edits.length, 1, "exactly one coalesced edit after the interval");
  assert.match(fake.last(), /5× tools/, "shows the latest step count, not five edits");
});

test("the heartbeat keeps the bubble alive over time", async () => {
  const fake = makeSink();
  const c = condenser(fake, { appearAfterMs: 10_000, minEditIntervalMs: 20, heartbeatMs: 25 });
  c.onInbound(U);
  c.onProgress(U, { kind: "tool", tool: "bash" });
  await delay(10);
  await delay(90);
  assert.ok(fake.edits.length >= 2, `heartbeat must re-edit (got ${fake.edits.length})`);
});

test("an edit identical to the last is skipped", async () => {
  const fake = makeSink();
  // Large heartbeat so the spinner can't advance and change the bytes.
  const c = condenser(fake, { appearAfterMs: 10_000, minEditIntervalMs: 20, heartbeatMs: 10_000 });
  c.onInbound(U);
  c.onProgress(U, { kind: "tool", tool: "bash" }); // materialize, steps=1
  await delay(40);
  c.onProgress(U, { kind: "toolDone" }); // no action/step change → identical render
  await delay(40);
  assert.equal(fake.edits.length, 0, "byte-identical render is not edited");
});

test("approval pauses the bubble on a ⏸ line and suspends the heartbeat", async () => {
  const fake = makeSink();
  const c = condenser(fake, { appearAfterMs: 10_000, minEditIntervalMs: 20, heartbeatMs: 25 });
  c.onInbound(U);
  c.onProgress(U, { kind: "tool", tool: "bash" });
  await delay(10);
  c.onApprovalRequested(U, "bash");
  await delay(40);
  assert.match(fake.last(), /^⏸ waiting for your approval: bash/);
  assert.doesNotMatch(fake.last(), /⏱/, "no liveness line while blocked");
  const afterPause = fake.edits.length;
  await delay(90);
  assert.equal(fake.edits.length, afterPause, "heartbeat is paused while blocked");
  c.onApprovalSettled(U);
  await delay(40);
  assert.match(fake.last(), /⚙️ working/, "resumes Working after the decision");
});

test("onReply finalizes to a ✅ done footer and stops the turn", async () => {
  const fake = makeSink();
  const c = condenser(fake, { appearAfterMs: 10_000, minEditIntervalMs: 20, heartbeatMs: 10_000 });
  c.onInbound(U);
  c.onProgress(U, { kind: "tool", tool: "bash" });
  await delay(10);
  c.onReply(U);
  await delay(10);
  assert.match(fake.last(), /^✅ done · \d/, "collapses to a terminal line");
  assert.match(fake.last(), /1× tools/);
  const afterDone = fake.edits.length;
  c.onProgress(U, { kind: "tool", tool: "late" }); // post-terminal — ignored
  await delay(40);
  assert.equal(fake.edits.length, afterDone, "no edits after the turn is done");
});

test("a StatusRateLimited rejection suppresses edits for retryAfterMs", async () => {
  const fake = makeSink();
  let throwOnce = true;
  fake.setOnEdit(() => {
    if (throwOnce) { throwOnce = false; throw new StatusRateLimited(120); }
  });
  const c = condenser(fake, { appearAfterMs: 10_000, minEditIntervalMs: 10, heartbeatMs: 15 });
  c.onInbound(U);
  c.onProgress(U, { kind: "tool", tool: "bash" });
  await delay(30); // first heartbeat edit throws 429(120ms) → suppress
  const attemptsDuring = fake.edits.length;
  await delay(60); // still inside the 120ms backoff window
  assert.equal(fake.edits.length, attemptsDuring, "no edit attempts during backoff");
  await delay(120); // past the backoff
  assert.ok(fake.edits.length > attemptsDuring, "edits resume after retryAfterMs");
});

test("onForceEnd collapses to ⏹ ended and tears down", async () => {
  const fake = makeSink();
  const c = condenser(fake, { appearAfterMs: 10_000, minEditIntervalMs: 20, heartbeatMs: 10_000 });
  c.onInbound(U);
  c.onProgress(U, { kind: "tool", tool: "bash" });
  await delay(10);
  c.onForceEnd(U);
  await delay(10);
  assert.match(fake.last(), /^⏹ ended · \d/);
});

test("the quiet-time safety cap self-finalizes a wedged turn", async () => {
  const fake = makeSink();
  const c = condenser(fake, { appearAfterMs: 10_000, minEditIntervalMs: 10, heartbeatMs: 15 }, 40);
  c.onInbound(U);
  c.onProgress(U, { kind: "tool", tool: "bash" });
  await delay(10);
  await delay(120); // exceed the 40ms quiet-time cap with no further activity
  assert.match(fake.last(), /^⏹ ended/, "safety cap finalizes the bubble");
});

test("a queued double-send gets its own bubble per turn", async () => {
  const fake = makeSink();
  const c = condenser(fake, { appearAfterMs: 10_000, minEditIntervalMs: 20, heartbeatMs: 10_000 });
  c.onInbound(U);
  c.onInbound(U); // pending = 2
  c.onProgress(U, { kind: "tool", tool: "a" });
  await delay(10);
  c.onReply(U); // finalize bubble A; one turn still queued
  await delay(10);
  c.onProgress(U, { kind: "tool", tool: "b" }); // materializes bubble B
  await delay(10);
  c.onReply(U);
  await delay(10);
  assert.equal(fake.sends.length, 2, "one status message per turn");
  const dones = fake.edits.filter((e) => e.text.startsWith("✅ done")).length;
  assert.equal(dones, 2, "both turns finalize");
});
