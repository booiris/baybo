import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";

import i18n from "./i18n";
import { sanitizeRestoredRows } from "./Transcript";
import { WorkBlockView } from "./WorkBlock";
import type { PersistedState, Row, WorkRow, WorkStep } from "./types";

/// The mirror seam, end to end: what `persistState` writes, through the JSON the
/// file actually holds, through `sanitizeRestoredRows`, into the label
/// `WorkBlockView` paints. `<Transcript>` itself is not mounted here: the seam
/// under test is the row model, which is where the two halves actually meet
/// (`Transcript.tsx:2431`), and mounting it costs the fake layout
/// `transcriptScroll.test.tsx` stands up (jsdom has none — see
/// docs/testing.md "web/") for nothing this suite would read.
///
/// It exists because the regression it pins lived in NEITHER half. `rows.test.ts`
/// pinned the restore's blanket `startedAt` strip as correct; `WorkBlock.test.tsx`
/// only ever fed the view rows that still had their anchor. Both suites were
/// green while every restored turn in the app read "3 steps" with its true
/// duration sitting unused on the row. A seam nobody crosses is a seam nobody
/// tests.

const at = (s: number): number => Date.UTC(2026, 7, 2, 12, 0, s);

const tool = (over: Partial<Extract<WorkStep, { kind: "tool" }>> = {}): WorkStep => ({
  kind: "tool",
  callId: "c1",
  label: "Bash",
  status: "ok",
  ...over,
});

const work = (over: Partial<WorkRow> & Pick<WorkRow, "id">): WorkRow => ({
  role: "work",
  steps: [],
  active: false,
  ...over,
});

/// Persist → the bytes on disk → restore. The JSON hop is not ceremony: the
/// mirror is a file, so anything the row model holds that does not survive
/// `JSON` is gone by the time the view sees it.
function throughMirror(rows: Row[]): Row[] {
  const state: PersistedState = {
    messages: rows,
    lastOrdinal: null,
    oldestOrdinal: null,
    hasMoreOlder: false,
  };
  const onDisk = JSON.parse(JSON.stringify(state)) as PersistedState;
  return sanitizeRestoredRows(onDisk.messages);
}

function paint(rows: Row[]): HTMLElement {
  const row = rows[0];
  if (row.role !== "work") throw new Error("expected a work row, got " + row.role);
  return render(
    <I18nextProvider i18n={i18n}>
      <WorkBlockView row={row} />
    </I18nextProvider>,
  ).container;
}

function headers(rows: Row[]): string[] {
  return [...paint(rows).querySelectorAll(".work-summary")].map((e) => e.textContent);
}

/// The live ticker's text, or `null` when it declines to render — which is what
/// a restored block with no anchor must produce.
function elapsedTicker(rows: Row[]): string | null {
  return paint(rows).querySelector(".work-elapsed")?.textContent ?? null;
}

describe("a turn's duration across the mirror", () => {
  // The reported regression, at the seam it actually broke: open an old chat
  // and every turn reads "N steps" (later "Worked") while `elapsedMs` — the
  // number the label used for three weeks — rides along untouched.
  it("a lone-run turn keeps its duration", () => {
    const rows = throughMirror([
      work({
        id: "w1",
        startedAt: at(0),
        elapsedMs: 167_000,
        steps: [{ kind: "reasoning", text: "thinking" }, tool(), tool({ callId: "c2" })],
      }),
    ]);
    expect(headers(rows)).toEqual(["Worked 2m 47s›"]);
  });

  // Per-run timing rides on the STEPS' own `at`, which the mirror never
  // stripped — so a stamped ladder keeps every rung's real duration.
  it("a stamped ladder keeps each run's own duration", () => {
    const rows = throughMirror([
      work({
        id: "w1",
        startedAt: at(0),
        elapsedMs: 60_000,
        steps: [
          { kind: "reasoning", text: "thinking", at: at(2) },
          { kind: "prose", text: "我先找一下", at: at(12) },
          tool({ callId: "c1", label: "Grep", at: at(20) }),
          { kind: "prose", text: "找到了", at: at(52) },
          tool({ callId: "c2", label: "Edit", at: at(55) }),
        ],
      }),
    ]);
    expect(headers(rows)).toEqual(["Worked 12s›", "Worked 40s›", "Worked 8s›"]);
  });

  // Unstamped ladder: the runs are each shorter than the block, so the block's
  // total belongs to none of them and there is nothing honest to show. It must
  // say "Worked" and stop — never a step count, which is what the run expands
  // to show anyway.
  it("an unstamped ladder says Worked, never a step count", () => {
    const rows = throughMirror([
      work({
        id: "w1",
        startedAt: at(0),
        elapsedMs: 60_000,
        steps: [
          { kind: "reasoning", text: "thinking" },
          { kind: "prose", text: "说点什么" },
          tool(),
        ],
      }),
    ]);
    const labels = headers(rows);
    expect(labels).toEqual(["Worked›", "Worked›"]);
    expect(labels.join()).not.toMatch(/step/i);
  });
});

describe("the anchor a restored block may NOT keep", () => {
  // Why the strip exists at all. A block still running when the app was killed
  // comes back live, and `LiveElapsed` renders `now − startedAt`: with the
  // anchor restored, a turn interrupted a week ago reads as a week of work.
  const weekOld = (): WorkRow =>
    work({ id: "w1", active: true, startedAt: Date.now() - 7 * 24 * 3600_000, steps: [tool({ status: "running" })] });

  it("an ACTIVE block loses it, so no ticker counts the app-closed hours", () => {
    expect(elapsedTicker(throughMirror([weekOld()]))).toBeNull();
  });

  it("and that is not vacuous — the same row unrestored does count them", () => {
    expect(elapsedTicker([weekOld()])).toMatch(/\d+h/);
  });

  // The narrowing: a CLOSED block runs neither the ticker nor the close path, so
  // there was never anything for the strip to protect there — only the per-run
  // bounds it cost.
  it("a CLOSED block keeps it", () => {
    const rows = throughMirror([
      work({ id: "w1", startedAt: at(0), elapsedMs: 5_000, steps: [tool()] }),
    ]);
    expect((rows[0] as WorkRow).startedAt).toBe(at(0));
  });
});
