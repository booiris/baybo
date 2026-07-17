import { afterEach, describe, expect, it, vi } from "vitest";
import {
  clearAwaitingApproval,
  foldAdjacentWork,
  foldTerminalNoticeIn,
  freezeActiveWork,
  hasUntimedWork,
  isStopAckNotice,
  isStopCommand,
  mergeWorkSteps,
  openWorkIn,
  reconcileWork,
  restStepToWork,
  restoreImageDims,
  sameTurnWorkIndex,
  sanitizeRestoredRows,
  transcriptItemToRow,
  wireStepToWork,
} from "../Transcript";
import type { Row, TranscriptRowItem, WorkRow, WorkStep } from "../types";

const NOW = 1_700_000_000_000;

function work(over: Partial<WorkRow> & Pick<WorkRow, "id">): WorkRow {
  return { role: "work", steps: [], active: false, ...over };
}

function tool(over: Partial<Extract<WorkStep, { kind: "tool" }>> = {}): WorkStep {
  return { kind: "tool", callId: "c1", label: "Bash", status: "running", ...over };
}

afterEach(() => {
  vi.useRealTimers();
});

describe("sanitizeRestoredRows — the four restore heals", () => {
  it("STRIPS a stale 'sending' state (the leg is gone — nothing is in flight)", () => {
    const rows: Row[] = [{ id: "p1", role: "user", content: "hi", sendState: "sending" }];
    expect(sanitizeRestoredRows(rows)).toEqual([{ id: "p1", role: "user", content: "hi", sendState: undefined }]);
  });

  it("KEEPS a 'failed' state — it is a real outcome, and its retry dot must survive", () => {
    const rows: Row[] = [{ id: "p1", role: "user", content: "hi", sendState: "failed" }];
    expect(sanitizeRestoredRows(rows)[0]).toMatchObject({ sendState: "failed" });
  });

  it("STRIPS startedAt — a persisted client clock would count the app-closed hours as 'Worked 7h'", () => {
    const rows: Row[] = [work({ id: "w1", steps: [{ kind: "status", text: "reading" }], startedAt: 1, elapsedMs: 5_000 })];
    const [out] = sanitizeRestoredRows(rows) as WorkRow[];
    expect(out.startedAt).toBeUndefined();
    expect(out.elapsedMs).toBe(5_000);
  });

  it("keeps a block that was live at persist LIVE (re-entry mid-turn must not collapse it)", () => {
    const rows: Row[] = [work({ id: "w1", steps: [tool()], active: true, startedAt: 1 })];
    expect(sanitizeRestoredRows(rows)[0]).toMatchObject({ active: true, startedAt: undefined });
  });

  it("CLEARS awaitingApproval — nothing can ever clear a badge whose prompt died with the app", () => {
    const rows: Row[] = [work({ id: "w1", steps: [tool({ awaitingApproval: "prompt-1" })] })];
    const [out] = sanitizeRestoredRows(rows) as WorkRow[];
    expect(out.steps[0]).toMatchObject({ kind: "tool", awaitingApproval: undefined });
  });

  it("leaves an already-decided approval label alone (it is the persisted verdict)", () => {
    const rows: Row[] = [work({ id: "w1", steps: [tool({ status: "ok", approval: "deny" })] })];
    const [out] = sanitizeRestoredRows(rows) as WorkRow[];
    expect(out.steps[0]).toMatchObject({ approval: "deny" });
  });

  it("FOLDS a legacy [work][work] adjacency into one card — a healthy turn always has a message between", () => {
    const rows: Row[] = [
      work({ id: "w1", steps: [{ kind: "reasoning", text: "a" }], elapsedMs: 1_000 }),
      work({ id: "w2", steps: [{ kind: "reasoning", text: "b" }], active: true, startedAt: 9 }),
    ];
    const out = sanitizeRestoredRows(rows) as WorkRow[];
    expect(out).toHaveLength(1);
    // Keeps the anchoring block's duration: dropping it is unrecoverable (the
    // fold re-persists as one row, and a cursor past the block means no sync
    // ever re-delivers it to re-time), and reads as "worked for a moment".
    expect(out[0]).toMatchObject({ id: "w1", active: true, startedAt: undefined, elapsedMs: 1_000 });
    expect(out[0].steps).toEqual([
      { kind: "reasoning", text: "a" },
      { kind: "reasoning", text: "b" },
    ]);
  });

  it("falls back to the stranded half's duration when the anchoring block has none", () => {
    const rows: Row[] = [
      work({ id: "w1", steps: [{ kind: "reasoning", text: "a" }] }),
      work({ id: "w2", steps: [{ kind: "status", text: "b" }], elapsedMs: 7_000 }),
    ];
    expect((sanitizeRestoredRows(rows)[0] as WorkRow).elapsedMs).toBe(7_000);
  });

  it("re-homes a durable progress block a prior build stranded BELOW its turn's answer", () => {
    const rows: Row[] = [
      work({ id: "w9", steps: [{ kind: "reasoning", text: "think" }] }),
      { id: "m12", role: "assistant", content: "done" },
      work({ id: "w10", steps: [{ kind: "status", text: "reading files" }] }),
    ];
    const out = sanitizeRestoredRows(rows);
    expect(out).toHaveLength(2);
    expect(out[0]).toMatchObject({ id: "w9" });
    expect((out[0] as WorkRow).steps).toEqual([
      { kind: "reasoning", text: "think" },
      { kind: "status", text: "reading files" },
    ]);
  });

  // Mirror of the adjacency fold's `active: prev.active || r.active`: a stranded
  // block that was still LIVE at persist must not be folded into a frozen target
  // and read as "worked" — the turn is still running.
  it("re-homes the STILL-LIVE bit with the stranded block, not just its steps", () => {
    const rows: Row[] = [
      work({ id: "w9", steps: [{ kind: "reasoning", text: "think" }], active: false }),
      { id: "m12", role: "assistant", content: "done" },
      work({ id: "w10", steps: [{ kind: "status", text: "still reading" }], active: true, startedAt: 9 }),
    ];
    const out = sanitizeRestoredRows(rows);
    expect(out).toHaveLength(2);
    expect(out[0]).toMatchObject({ id: "w9", active: true });
  });

  it("does NOT re-home into a genuinely earlier turn (the trailing answer is older than the block)", () => {
    const rows: Row[] = [
      work({ id: "w9", steps: [{ kind: "reasoning", text: "think" }] }),
      { id: "m5", role: "assistant", content: "older" },
      work({ id: "w10", steps: [{ kind: "status", text: "later turn" }] }),
    ];
    expect(sanitizeRestoredRows(rows)).toHaveLength(3);
  });

  it("drops an empty work block — it has nothing to show", () => {
    expect(sanitizeRestoredRows([work({ id: "w1" })])).toEqual([]);
  });

  it("drops a row whose role it does not render", () => {
    const rows = [{ id: "x", role: "future", content: "?" }] as unknown as Row[];
    expect(sanitizeRestoredRows(rows)).toEqual([]);
  });

  it("tolerates an absent mirror", () => {
    expect(sanitizeRestoredRows(undefined)).toEqual([]);
  });
});

describe("transcriptItemToRow — three wire shapes, one Row", () => {
  it("keys a user row by its platform_msg_id so the optimistic bubble reconciles", () => {
    const item: TranscriptRowItem = { id: "m4", ordinal: 4, kind: "message", role: "user", text: "hi", platform_msg_id: "pm-1" };
    expect(transcriptItemToRow(item)).toEqual({ id: "pm-1", role: "user", content: "hi", attachments: undefined });
  });

  it("keys an assistant row by the stable m<ordinal> id and carries its attachments", () => {
    const attachments = [{ kind: "image" as const, blob_id: "sha256:ab.tok", mime_type: "image/png", size: 12 }];
    const item: TranscriptRowItem = { id: "m5", ordinal: 5, kind: "message", role: "assistant", text: "there", attachments };
    expect(transcriptItemToRow(item)).toEqual({ id: "m5", role: "assistant", content: "there", attachments });
  });

  it("DROPS a persisted /stop — the button issues it, it is never a chat bubble", () => {
    const item: TranscriptRowItem = { id: "m6", kind: "message", role: "user", text: "/stop" };
    expect(transcriptItemToRow(item)).toBeNull();
  });

  it("renders a /stop acknowledgement as the compact stopped indicator, not its raw text", () => {
    const item: TranscriptRowItem = { id: "n3", kind: "notice", text: "Stopped.\n- Cancelled the in-progress reply." };
    expect(transcriptItemToRow(item)).toEqual({ id: "n3", role: "notice", content: "", stopped: true });
  });

  it("keeps an ordinary notice as its own centered row", () => {
    const item: TranscriptRowItem = { id: "n4", kind: "notice", text: "degraded mode" };
    expect(transcriptItemToRow(item)).toEqual({ id: "n4", role: "notice", content: "degraded mode" });
  });

  it("anchors a work row to the SERVER's turn start and duration", () => {
    const item: TranscriptRowItem = {
      id: "w7",
      ordinal: 7,
      kind: "work",
      steps: [{ kind: "reasoning", text: "hmm" }],
      work_started_at: "2026-07-12T10:00:00.000Z",
      work_ended_at: "2026-07-12T10:00:12.000Z",
    };
    expect(transcriptItemToRow(item)).toEqual({
      id: "w7",
      role: "work",
      steps: [{ kind: "reasoning", text: "hmm" }],
      active: false,
      startedAt: Date.parse("2026-07-12T10:00:00.000Z"),
      elapsedMs: 12_000,
    });
  });

  it("leaves an unfinished work row untimed rather than inventing a duration", () => {
    const item: TranscriptRowItem = { id: "w8", kind: "work", steps: [{ kind: "prose", text: "x" }], work_started_at: "2026-07-12T10:00:00.000Z" };
    expect(transcriptItemToRow(item)).toMatchObject({ elapsedMs: undefined });
  });
});

describe("wireStepToWork — the subscribe_state shape", () => {
  it("KEEPS call_id: without it every live tool step in a resumed turn hangs at 'running' forever", () => {
    expect(wireStepToWork({ kind: "tool", call_id: "call-9", tool: "Bash", label: "Bash(ls)" })).toEqual({
      kind: "tool",
      callId: "call-9",
      label: "Bash(ls)",
      status: "running",
      summary: undefined,
      approval: undefined,
    });
  });

  it("falls back to the tool name when the step carries no label", () => {
    expect(wireStepToWork({ kind: "tool", call_id: "c", tool: "Read" })).toMatchObject({ label: "Read" });
  });

  it("carries a status/summary/approval the call already finished with inside the buffered turn", () => {
    expect(wireStepToWork({ kind: "tool", call_id: "c", tool: "Bash", status: "ok", summary: "3 files", approval: "approve" })).toMatchObject({
      status: "ok",
      summary: "3 files",
      approval: "approve",
    });
  });

  it("maps a text step by kind", () => {
    expect(wireStepToWork({ kind: "status", text: "reading" })).toEqual({ kind: "status", text: "reading" });
    expect(wireStepToWork({ kind: "reasoning" })).toEqual({ kind: "reasoning", text: "" });
  });
});

describe("restStepToWork — the REST shape (tool_* names)", () => {
  it("reads call_id / tool_label / tool_status / tool_summary", () => {
    expect(
      restStepToWork({ kind: "tool", call_id: "c1", tool: "Bash", tool_label: "Bash(ls)", tool_status: "error", tool_summary: "exit 1" }),
    ).toEqual({
      kind: "tool",
      callId: "c1",
      label: "Bash(ls)",
      status: "error",
      summary: "exit 1",
      approval: undefined,
    });
  });

  it("tolerates a row persisted before the gateway sent call_id", () => {
    expect(restStepToWork({ kind: "tool", tool: "Bash" })).toMatchObject({ callId: "" });
  });

  it("defaults a reconstructed call to 'ok' — its call is closed by definition", () => {
    expect(restStepToWork({ kind: "tool", tool: "Read" })).toMatchObject({ label: "Read", status: "ok" });
  });

  it("keeps the persisted approval verdict so a reload re-labels the same step", () => {
    expect(restStepToWork({ kind: "tool", tool: "Bash", approval: "deny" })).toMatchObject({ approval: "deny" });
  });
});

describe("isStopCommand / isStopAckNotice", () => {
  it.each([
    ["/stop", true],
    ["  /stop  ", true],
    ["/STOP", true],
    ["/stop@baybo", true],
    ["/stop now", true],
    ["stop", false],
    ["/stopwatch", false],
    ["please /stop", false],
    ["", false],
  ])("isStopCommand(%j) === %s", (text, expected) => {
    expect(isStopCommand(text)).toBe(expected);
  });

  it.each([
    ["Stopped.\n- Cancelled the in-progress reply.", true],
    ["Nothing in progress to stop.", true],
    ["  Stopped. ", true],
    ["Stopped the car", false],
    ["all good", false],
  ])("isStopAckNotice(%j) === %s", (text, expected) => {
    expect(isStopAckNotice(text)).toBe(expected);
  });
});

describe("mergeWorkSteps", () => {
  it("collapses two representations of one turn instead of doubling every step", () => {
    const live: WorkStep[] = [{ kind: "reasoning", text: "a" }, tool({ callId: "c1", status: "running" })];
    const recon: WorkStep[] = [{ kind: "reasoning", text: "a" }, tool({ callId: "c1", status: "ok" })];
    expect(mergeWorkSteps(live, recon)).toEqual(live);
  });

  it("appends a torn turn's disjoint half", () => {
    const a: WorkStep[] = [{ kind: "reasoning", text: "a" }];
    const b: WorkStep[] = [{ kind: "prose", text: "b" }];
    expect(mergeWorkSteps(a, b)).toEqual([...a, ...b]);
  });

  it("keys a tool step by its call id, not its label — the same call re-labelled is still one step", () => {
    const a: WorkStep[] = [tool({ callId: "c1", label: "Bash" })];
    const b: WorkStep[] = [tool({ callId: "c1", label: "Bash(ls -la)" })];
    expect(mergeWorkSteps(a, b)).toHaveLength(1);
  });

  // A reconstructed step has NO call id (ChatWorkStep drops it), so keying those
  // `tool:` collapsed every one of them to a single identity — folding two
  // reconstructed halves kept the first tool step and deleted the rest. A real
  // 88-row turn lost 32 steps to this on every restore.
  it("keeps every id-less tool step when folding two reconstructed halves", () => {
    const a: WorkStep[] = [
      tool({ callId: "", label: "Now", status: "ok", summary: "12:00" }),
      tool({ callId: "", label: "Fetch(a)", status: "error", summary: "404" }),
    ];
    const b: WorkStep[] = [
      tool({ callId: "", label: "Fetch(b)", status: "ok", summary: "body" }),
      tool({ callId: "", label: "Bash(ls)", status: "ok", summary: "out" }),
    ];
    expect(mergeWorkSteps(a, b)).toEqual([...a, ...b]);
  });

  it("still collapses an id-less step re-delivered as itself (the same block synced twice)", () => {
    const steps: WorkStep[] = [tool({ callId: "", label: "Bash(ls)", status: "ok", summary: "out" })];
    expect(mergeWorkSteps(steps, [...steps])).toEqual(steps);
  });

  it("distinguishes id-less steps that share a label but differ in outcome", () => {
    const a: WorkStep[] = [tool({ callId: "", label: "Fetch(x)", status: "error", summary: "404" })];
    const b: WorkStep[] = [tool({ callId: "", label: "Fetch(x)", status: "ok", summary: "body" })];
    expect(mergeWorkSteps(a, b)).toHaveLength(2);
  });

  // The residual loss for a row that predates the gateway sending `call_id`:
  // two id-less calls identical in label, status AND summary are
  // indistinguishable and still collapse. Far smaller than collapsing all of
  // them, and it cannot happen to a step the current gateway reconstructs.
  it("collapses two id-less steps identical in label, status and summary", () => {
    const step = tool({ callId: "", label: "Bash(ls)", status: "ok", summary: "out" });
    expect(mergeWorkSteps([step], [{ ...step }])).toHaveLength(1);
  });

  // Now that the REST shape carries `call_id`, a live block and its OWN
  // reconstruction agree on identity. They meet on every sync that lands during
  // a live turn (the REPLACE fuse, the difference merge) — keyed apart, every
  // tool step in the card rendered twice.
  it("collapses a live step against its own reconstruction instead of doubling it", () => {
    const live: WorkStep[] = [tool({ callId: "call_a", label: "Bash(ls)", status: "running" })];
    const recon: WorkStep[] = [tool({ callId: "call_a", label: "Bash(ls)", status: "ok", summary: "out" })];
    expect(mergeWorkSteps(recon, live)).toEqual(recon);
  });
});

describe("freezeActiveWork", () => {
  it("freezes EVERY still-active block — there is only ever one in-flight turn", () => {
    vi.useFakeTimers();
    vi.setSystemTime(NOW);
    const rows: Row[] = [
      work({ id: "w1", steps: [tool()], active: true, startedAt: NOW - 4_000 }),
      { id: "m1", role: "assistant", content: "x" },
      work({ id: "w2", steps: [tool()], active: true, startedAt: NOW - 1_000 }),
    ];
    const out = freezeActiveWork(rows) as [WorkRow, Row, WorkRow];
    expect(out[0]).toMatchObject({ active: false, elapsedMs: 4_000 });
    expect(out[2]).toMatchObject({ active: false, elapsedMs: 1_000 });
  });

  it("prefers a duration already reconciled from the server over the wall clock", () => {
    const rows: Row[] = [work({ id: "w1", steps: [tool()], active: true, startedAt: 1, elapsedMs: 250 })];
    expect((freezeActiveWork(rows)[0] as WorkRow).elapsedMs).toBe(250);
  });
});

describe("hasUntimedWork — the mirror we broke, and can only rebuild from the gateway", () => {
  it("flags a CLOSED block with no duration — the number is gone and only the gateway has it", () => {
    expect(hasUntimedWork([work({ id: "w3", steps: [{ kind: "status", text: "reading" }] })])).toBe(true);
  });

  it("ignores an ACTIVE block — it is still running, so having no duration is correct", () => {
    const rows: Row[] = [work({ id: "w3", steps: [tool()], active: true, startedAt: 1 })];
    expect(hasUntimedWork(rows)).toBe(false);
  });

  it("ignores a timed block — the overwhelming majority, which must keep differencing", () => {
    const rows: Row[] = [work({ id: "w3", steps: [tool()], elapsedMs: 4_000 })];
    expect(hasUntimedWork(rows)).toBe(false);
  });

  it("ignores an empty block — sanitizeRestoredRows drops it, so it never renders untimed", () => {
    expect(hasUntimedWork([work({ id: "w3" })])).toBe(false);
  });

  it("tolerates an absent mirror, and on-disk garbage (the mirror is JSON, not a trusted type)", () => {
    expect(hasUntimedWork(undefined)).toBe(false);
    expect(hasUntimedWork([{ id: "w3", role: "work", active: false } as unknown as Row])).toBe(false);
  });

  it("finds a broken block anywhere in the thread, not just at the tail", () => {
    const rows: Row[] = [
      work({ id: "w3", steps: [{ kind: "status", text: "reading" }] }),
      { id: "m91", role: "assistant", content: "done" },
      work({ id: "w95", steps: [tool()], elapsedMs: 4_000 }),
    ];
    expect(hasUntimedWork(rows)).toBe(true);
  });
});

describe("openWorkIn / foldTerminalNoticeIn — one turn, one card", () => {
  const steps = (rows: Row[]) => rows.filter((r): r is WorkRow => r.role === "work");

  it("folds a straggler into a FROZEN tail — the invariant against the [work][work] split", () => {
    // `turn_state{inactive}` races ahead of a cancel's unguarded
    // `tool_completed`, so the block is already closed when the frame lands.
    let rows: Row[] = openWorkIn([], (w) => ({ ...w, steps: [tool({ callId: "c1" })] }));
    rows = freezeActiveWork(rows);
    rows = openWorkIn(rows, (w) => ({ ...w, steps: [...w.steps, tool({ callId: "c2" })] }));

    expect(steps(rows)).toHaveLength(1);
    expect(steps(rows)[0].steps).toHaveLength(2);
  });

  it("folds a terminal notice INTO an active block rather than severing it", () => {
    let rows: Row[] = openWorkIn([], (w) => ({ ...w, steps: [tool({ callId: "c1" })] }));
    rows = foldTerminalNoticeIn(rows, "warn", "degraded");
    rows = openWorkIn(rows, (w) => ({ ...w, steps: [...w.steps, tool({ callId: "c2" })] }));

    expect(steps(rows)).toHaveLength(1);
    expect(steps(rows)[0].steps).toMatchObject([{ kind: "tool" }, { kind: "notice", text: "degraded" }, { kind: "tool" }]);
  });

  it("keeps ONE card when a terminal notice lands on a FROZEN block mid-turn", () => {
    // The straggler sequence of the first case, with a terminal notice (a
    // server rejection, the empty-user-reply warn) in between. The notice sees
    // `active:false` so it keeps its own row — correct, it may be durable — but
    // that row breaks the ADJACENCY `openWorkIn`'s fold-into-frozen-tail
    // invariant rests on, so the straggler forks a SECOND card.
    // Wanted: [work][notice] with the straggler folded back into the one block.
    let rows: Row[] = openWorkIn([], (w) => ({ ...w, steps: [tool({ callId: "c1" })] }));
    rows = freezeActiveWork(rows);
    rows = foldTerminalNoticeIn(rows, "warn", "degraded");
    rows = openWorkIn(rows, (w) => ({ ...w, steps: [...w.steps, tool({ callId: "c2" })] }));

    expect(rows.filter((r) => r.role === "notice")).toHaveLength(1);
    expect(steps(rows)).toHaveLength(1);
    expect(steps(rows)[0].steps).toHaveLength(2);
  });

  it("stops the back-scan at the turn's answer — a LATER turn still gets its own card", () => {
    // The back-scan crosses notices only. A finished turn is separated from the
    // next by its answer bubble (or, for a turn that produced none, the next
    // turn's user row), so a notice sitting between turns can't make a new
    // turn's first frame fold back into the previous turn's card.
    let rows: Row[] = openWorkIn([], (w) => ({ ...w, steps: [tool({ callId: "c1" })] }));
    rows = freezeActiveWork(rows);
    rows = [...rows, { id: "m1", role: "assistant", content: "done" }];
    rows = foldTerminalNoticeIn(rows, "info", "compacted");
    rows = openWorkIn(rows, (w) => ({ ...w, steps: [tool({ callId: "c2" })] }));

    expect(rows.map((r) => r.role)).toEqual(["work", "assistant", "notice", "work"]);
    expect(steps(rows)[0].steps).toHaveLength(1);
  });
});

describe("foldAdjacentWork — a turn cut by a page boundary is still one turn", () => {
  // The real numbers off a device mirror: a 91-row session whose turn ran rows
  // 3..90. The baseline sync page is the newest 50 rows (42..91), which carries
  // no user row, so its half opened at row 43 and was closed by the answer at
  // row 91; the history page (rows 1..41) held the user row, so its half timed
  // from the real turn start and was closed by that page's trailing flush at
  // row 41. Prepending the older page puts them side by side.
  const TURN_START = 1784173283804; // row 2 — the user's prompt
  const FIRST_END = 1784173863833; // row 41 — the older page's last row
  const SECOND_START = 1784173872143; // row 43
  const TURN_END = 1784174854449; // row 91 — the answer
  const first = (): WorkRow =>
    work({ id: "w3", steps: [tool({ callId: "", label: "Fetch(a)" })], startedAt: TURN_START, elapsedMs: FIRST_END - TURN_START });
  const second = (): WorkRow =>
    work({ id: "w43", steps: [tool({ callId: "", label: "Fetch(b)" })], startedAt: SECOND_START, elapsedMs: TURN_END - SECOND_START });

  it("spans the whole turn — neither half's own duration is the turn's", () => {
    const [row] = foldAdjacentWork([first(), second()]) as [WorkRow];
    expect(row.startedAt).toBe(TURN_START);
    expect(row.elapsedMs).toBe(TURN_END - TURN_START); // 26m11s, not the halves' 9m40s / 16m22s
  });

  it("keeps one card, the earlier id, and every step from both halves", () => {
    const out = foldAdjacentWork([first(), second()]) as WorkRow[];
    expect(out).toHaveLength(1);
    expect(out[0].id).toBe("w3");
    expect(out[0].steps).toHaveLength(2);
  });

  it("stays live when the newer half is still running, anchored at the turn's start", () => {
    const live = { ...second(), active: true, elapsedMs: undefined };
    const [row] = foldAdjacentWork([first(), live]) as [WorkRow];
    expect(row).toMatchObject({ active: true, startedAt: TURN_START });
  });

  it("leaves a healthy thread alone — a turn's block is separated by its answer", () => {
    const rows: Row[] = [first(), { id: "m91", role: "assistant", content: "done" }, second()];
    expect(foldAdjacentWork(rows)).toHaveLength(3);
  });

  it("is idempotent, so it is safe to run at every seam", () => {
    const once = foldAdjacentWork([first(), second()]);
    expect(foldAdjacentWork(once)).toEqual(once);
  });

  it("collapses a run of three halves (a turn spanning three pages)", () => {
    const third = work({ id: "w80", steps: [tool({ callId: "", label: "Fetch(c)" })], startedAt: SECOND_START, elapsedMs: TURN_END - SECOND_START });
    const out = foldAdjacentWork([first(), second(), third]) as WorkRow[];
    expect(out).toHaveLength(1);
    expect(out[0]).toMatchObject({ id: "w3", startedAt: TURN_START, elapsedMs: TURN_END - TURN_START });
  });

  // The other fold: a live block beside its OWN reconstruction is one span in
  // two representations, so the server's timing replaces the client's guess —
  // it must NOT be spanned as if the two were sequential.
  it("reconciles a live block with its own reconstruction rather than spanning it", () => {
    const live = work({ id: "u7-abc", steps: [tool({ callId: "c1" })], active: true, startedAt: 999 });
    const recon = work({ id: "w43", steps: [tool({ callId: "", label: "Fetch(b)" })], startedAt: SECOND_START, elapsedMs: 4_000 });
    const [row] = foldAdjacentWork([live, recon]) as [WorkRow];
    expect(row).toMatchObject({ id: "u7-abc", active: true, startedAt: SECOND_START, elapsedMs: 4_000 });
  });

  it("reconciles the same block re-delivered (one id, one span)", () => {
    const [row] = foldAdjacentWork([first(), first()]) as [WorkRow];
    expect(row).toMatchObject({ id: "w3", startedAt: TURN_START, elapsedMs: FIRST_END - TURN_START });
    expect(row.steps).toHaveLength(1);
  });
});

describe("reconcileWork", () => {
  it("keeps the live block's id + active state, adopts the server's timing, unions the steps", () => {
    const base = work({ id: "live-uid", steps: [tool({ callId: "c1" })], active: true, startedAt: 999 });
    const recon = work({ id: "w7", steps: [{ kind: "reasoning", text: "r" }], startedAt: 100, elapsedMs: 8_000 });
    expect(reconcileWork(base, recon)).toEqual({
      id: "live-uid",
      role: "work",
      active: true,
      startedAt: 100,
      elapsedMs: 8_000,
      steps: [tool({ callId: "c1" }), { kind: "reasoning", text: "r" }],
    });
  });

  it("falls back to the live anchors when the server carries none", () => {
    const base = work({ id: "live", steps: [], active: true, startedAt: 999, elapsedMs: 5 });
    expect(reconcileWork(base, work({ id: "w1" }))).toMatchObject({ startedAt: 999, elapsedMs: 5 });
  });
});

describe("sameTurnWorkIndex", () => {
  it("finds the turn's block above its answer bubble", () => {
    const rows: Row[] = [work({ id: "w9", steps: [tool()] }), { id: "m12", role: "assistant", content: "a" }];
    expect(sameTurnWorkIndex(rows, 10)).toBe(0);
  });

  it("scans back over trailing notices too", () => {
    const rows: Row[] = [
      work({ id: "w9", steps: [tool()] }),
      { id: "m12", role: "assistant", content: "a" },
      { id: "n1", role: "notice", content: "note" },
    ];
    expect(sameTurnWorkIndex(rows, 10)).toBe(0);
  });

  it("refuses when the trailing answer is OLDER than the block (a genuinely later turn)", () => {
    const rows: Row[] = [work({ id: "w9", steps: [tool()] }), { id: "m5", role: "assistant", content: "a" }];
    expect(sameTurnWorkIndex(rows, 10)).toBe(-1);
  });

  // The user row must BREAK the backward scan: it is the next turn's opening, so
  // a block below it belongs to that turn, not to the block above it. The
  // fixture carries an ordinal-NEWER answer (m12 > 10) deliberately — without it
  // the scan stops for want of a `sawTurnAnswer` instead of for want of a turn
  // boundary, and the assertion passes even when the `break` is gone.
  it("refuses when a user row breaks the trailing run", () => {
    const rows: Row[] = [
      work({ id: "w9", steps: [tool()] }),
      { id: "m12", role: "assistant", content: "a" },
      { id: "pm-1", role: "user", content: "next" },
    ];
    expect(sameTurnWorkIndex(rows, 10)).toBe(-1);
  });
});

describe("clearAwaitingApproval", () => {
  it("clears the badge on a blocked tool step", () => {
    expect(clearAwaitingApproval(tool({ awaitingApproval: "p1" }))).toMatchObject({ awaitingApproval: undefined });
  });

  it("passes a step with no badge, and any non-tool step, through untouched", () => {
    const clean = tool();
    expect(clearAwaitingApproval(clean)).toBe(clean);
    const text: WorkStep = { kind: "reasoning", text: "r" };
    expect(clearAwaitingApproval(text)).toBe(text);
  });
});

describe("restoreImageDims", () => {
  it("restores a usable size", () => {
    expect(restoreImageDims({ "sha256:ab": [800, 600] }).get("sha256:ab")).toEqual([800, 600]);
  });

  it.each([
    ["a zero width", [0, 600]],
    ["a zero height", [800, 0]],
    ["a negative dimension", [-800, 600]],
    ["NaN", [Number.NaN, 600]],
    ["Infinity", [Number.POSITIVE_INFINITY, 600]],
  ])("drops %s — it would poison the reserved box's ratio into a 0x0 reservation", (_label, dims) => {
    const raw = { "sha256:ab": dims as [number, number] };
    expect(restoreImageDims(raw).size).toBe(0);
  });

  it("drops on-disk garbage — the mirror is JSON, not a trusted type", () => {
    const raw = { a: "nope", b: ["800", "600"], c: null } as unknown as Record<string, [number, number]>;
    expect(restoreImageDims(raw).size).toBe(0);
  });

  it("tolerates an absent map (a mirror written before imageDims existed)", () => {
    expect(restoreImageDims(undefined).size).toBe(0);
  });
});
