import test from "node:test";
import assert from "node:assert/strict";

import { LarkStreamingSession } from "../dist/streaming.js";

const noopLogger = {
  debug() {},
  info() {},
  warn() {},
  error() {},
};

// Build a mock LarkChannel that captures stream() invocations and
// drives the producer to completion, recording each setContent call.
function stubChannel() {
  const calls = [];
  return {
    calls,
    async stream(_to, input) {
      const setContent = [];
      const controller = {
        messageId: "card-1",
        async append() {},
        async setContent(text) {
          setContent.push(text);
        },
      };
      calls.push({ to: _to, setContent });
      // Invoke the producer in this microtask so the test can drive it.
      await input.markdown(controller);
      return { messageId: "card-1" };
    },
  };
}

test("LarkStreamingSession: appends accumulate and finish() resolves", async () => {
  const channel = stubChannel();
  const session = new LarkStreamingSession(channel, "oc_x", noopLogger);

  // Yield the event loop so the SDK's producer starts. Append between
  // ticks; setContent calls should reflect the accumulated buffer.
  await Promise.resolve();
  session.append("Hel");
  await Promise.resolve();
  session.append("lo, ");
  await Promise.resolve();
  await session.finish("Hello, world!");

  assert.equal(channel.calls.length, 1);
  const [{ to, setContent }] = channel.calls;
  assert.equal(to, "oc_x");
  // The final setContent must equal the full body.
  assert.equal(setContent.at(-1), "Hello, world!");
});

test("LarkStreamingSession: finish() with no finalText keeps accumulated buffer", async () => {
  const channel = stubChannel();
  const session = new LarkStreamingSession(channel, "oc_x", noopLogger);

  await Promise.resolve();
  session.append("partial");
  await Promise.resolve();
  await session.finish();

  const setContent = channel.calls[0].setContent;
  assert.equal(setContent.at(-1), "partial");
});

test("LarkStreamingSession: setContent replaces buffer wholesale", async () => {
  const channel = stubChannel();
  const session = new LarkStreamingSession(channel, "oc_x", noopLogger);

  await Promise.resolve();
  session.append("draft");
  await Promise.resolve();
  session.setContent("final answer");
  await session.finish();

  const setContent = channel.calls[0].setContent;
  assert.equal(setContent.at(-1), "final answer");
});

test("LarkStreamingSession: append() after finish() is a no-op", async () => {
  const channel = stubChannel();
  const session = new LarkStreamingSession(channel, "oc_x", noopLogger);

  await Promise.resolve();
  session.append("done");
  await session.finish();
  // Late append must not throw or push more content.
  session.append("late");
  // Idempotent finish.
  await session.finish();

  const setContent = channel.calls[0].setContent;
  assert.equal(setContent.at(-1), "done");
});

test("LarkStreamingSession: stream() rejection surfaces from finish()", async () => {
  const failing = {
    async stream() {
      throw new Error("rate_limited");
    },
  };
  const session = new LarkStreamingSession(failing, "oc_x", noopLogger);

  await assert.rejects(() => session.finish("anything"), /rate_limited/);
});
