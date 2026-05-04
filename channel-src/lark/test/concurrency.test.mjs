import test from "node:test";
import assert from "node:assert/strict";

import { Semaphore } from "../dist/concurrency.js";

test("Semaphore: permits=1 strictly serializes", async () => {
  const sem = new Semaphore(1);
  const log = [];

  const tasks = ["a", "b", "c"].map((id) =>
    sem.withPermit(async () => {
      log.push(`enter:${id}`);
      // Yield so the next task gets a chance to acquire (which it
      // shouldn't, since we hold the only permit).
      await Promise.resolve();
      await Promise.resolve();
      log.push(`exit:${id}`);
      return id;
    }),
  );

  const out = await Promise.all(tasks);
  assert.deepEqual(out, ["a", "b", "c"]);
  assert.deepEqual(log, [
    "enter:a",
    "exit:a",
    "enter:b",
    "exit:b",
    "enter:c",
    "exit:c",
  ]);
});

test("Semaphore: permits=2 allows two concurrent, queues a third", async () => {
  const sem = new Semaphore(2);
  let active = 0;
  let peakActive = 0;
  const order = [];

  const block = async (id) => {
    order.push(`enter:${id}`);
    active += 1;
    if (active > peakActive) peakActive = active;
    // Multiple yields so peers can fight for the same slot.
    for (let i = 0; i < 4; i++) await Promise.resolve();
    active -= 1;
    order.push(`exit:${id}`);
  };

  await Promise.all(
    ["a", "b", "c", "d"].map((id) => sem.withPermit(() => block(id))),
  );

  assert.equal(peakActive, 2);
  // FIFO: a and b enter first, c only after one of them exits.
  assert.equal(order[0], "enter:a");
  assert.equal(order[1], "enter:b");
});

test("Semaphore: a thrown task still releases its permit", async () => {
  const sem = new Semaphore(1);
  await assert.rejects(
    () =>
      sem.withPermit(async () => {
        throw new Error("boom");
      }),
    /boom/,
  );
  // If the permit had leaked, the next acquire would hang. Resolve
  // within a short window to assert the slot is back.
  const got = await Promise.race([
    sem.withPermit(async () => "ok"),
    new Promise((resolve) => setTimeout(() => resolve("timeout"), 100)),
  ]);
  assert.equal(got, "ok");
});

test("Semaphore: rejects permits < 1 at construction", () => {
  assert.throws(() => new Semaphore(0), />= 1/);
  assert.throws(() => new Semaphore(-1), />= 1/);
});
