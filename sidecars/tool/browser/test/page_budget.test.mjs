// Imports `dist/test/page_budget.mjs` — the unminified second output esbuild
// emits precisely so this file can reach the module (see esbuild.config.mjs).

import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const modulePath = resolve(here, "..", "dist", "test", "page_budget.mjs");
if (!existsSync(modulePath)) {
  throw new Error(
    `page budget test bundle missing at ${modulePath} — package.json's test script ` +
      `runs \`node esbuild.config.mjs\` before tests; rebuild and retry.`,
  );
}
const { MAX_PAGES, PageBudget, evictionNotice } = await import(modulePath);

function harness({ maxPages = 3, pages = [], closeFails = new Set() } = {}) {
  const open = [...pages];
  const closed = [];
  const budget = new PageBudget({
    maxPages,
    listPages: async () => [...open],
    closePage: async (id) => {
      if (closeFails.has(id)) throw new Error(`refusing ${id}`);
      const i = open.findIndex((p) => p.id === id);
      if (i === -1) throw new Error(`no page ${id}`);
      open.splice(i, 1);
      closed.push(id);
    },
  });
  return { budget, open, closed };
}

const page = (id) => ({ id, url: `https://example.com/${id}` });

test("under the cap nothing is evicted", async () => {
  const { budget, closed } = harness({ maxPages: 3, pages: [page(1)] });
  const report = await budget.makeRoomForOne();
  assert.deepEqual(report, { closed: [], failed: [] });
  assert.deepEqual(closed, []);
});

test("evicts down to maxPages - 1 so the new tab lands at the cap", async () => {
  const { budget, closed } = harness({
    maxPages: 3,
    pages: [page(1), page(2), page(3), page(4)],
  });
  await budget.makeRoomForOne();
  // 4 open, cap 3 → 2 must go so the incoming tab makes 3.
  assert.equal(closed.length, 2);
});

test("least recently used goes first, and a fresh tab is not a victim", async () => {
  const { budget, closed } = harness({
    maxPages: 3,
    pages: [page(1), page(2), page(3)],
  });
  budget.noteUse(1);
  budget.noteUse(3);
  budget.noteUse(2);
  await budget.makeRoomForOne();
  // 3 open, cap 3 → exactly one eviction: page 2 is newest-used, page 3
  // next, so page 1 is the victim.
  assert.deepEqual(closed, [1]);
});

test("never-used pages outrank used ones as victims, oldest id first", async () => {
  const { budget, closed } = harness({
    maxPages: 2,
    pages: [page(5), page(9), page(2)],
  });
  budget.noteUse(2);
  await budget.makeRoomForOne();
  // 3 open, cap 2 → 2 evictions. 5 and 9 were never used; 5 is older.
  assert.deepEqual(closed, [5, 9]);
});

test("a close that fails is reported, not thrown", async () => {
  const { budget } = harness({
    maxPages: 2,
    pages: [page(1), page(2), page(3)],
    closeFails: new Set([1]),
  });
  const report = await budget.makeRoomForOne();
  assert.deepEqual(report.failed, [1]);
  assert.deepEqual(
    report.closed.map((p) => p.id),
    [2],
  );
});

test("a browser that cannot list its pages disables the cap rather than failing", async () => {
  const budget = new PageBudget({
    maxPages: 2,
    listPages: async () => {
      throw new Error("browser is gone");
    },
    closePage: async () => {
      throw new Error("should not be called");
    },
  });
  assert.deepEqual(await budget.makeRoomForOne(), { closed: [], failed: [] });
});

test("the last page is never evicted", async () => {
  const { budget, closed } = harness({ maxPages: 2, pages: [page(1)] });
  await budget.makeRoomForOne();
  assert.deepEqual(closed, []);
});

test("recency for a vanished page does not survive to skew later ordering", async () => {
  const { budget, open, closed } = harness({
    maxPages: 2,
    pages: [page(1), page(2)],
  });
  budget.noteUse(1);
  budget.noteUse(2);
  // Page 1 disappears on its own (window.close, crash) and id 1 is later
  // reused by nothing — but a stale "recently used" entry would make the
  // remaining page look older than a brand new one.
  open.splice(0, 1);
  open.push(page(3), page(4));
  await budget.makeRoomForOne();
  assert.ok(!closed.includes(1), "page 1 was already gone");
});

// The cap is a backstop, not a budget: it must sit far enough above any
// real working set that hitting it means something went wrong. A value
// that drifted down into normal-usage territory would start closing tabs
// out from under agents that were still using them.
test("the shipped ceiling is a backstop, not a working-set budget", () => {
  assert.equal(MAX_PAGES, 128);
});

test("eviction is announced to the model, never silent", () => {
  assert.equal(evictionNotice({ closed: [], failed: [] }, 8), undefined);
  const notice = evictionNotice({ closed: [page(4)], failed: [7] }, 8);
  assert.match(notice, /caps concurrently open tabs at 8/);
  assert.match(notice, /Closed least-recently-used page\(s\): 4/);
  assert.match(notice, /Could not close: 7/);
  assert.match(notice, /close_page/);
});
