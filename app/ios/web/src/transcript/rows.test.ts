import type { TFunction } from "i18next";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  applySyncReplace,
  clearAwaitingApproval,
  bundleAnswer,
  compactionDividerIds,
  dropInFlightAnswerStep,
  FIRST_PAINT_ROWS,
  flattenGloss,
  foldAdjacentWork,
  foldMidTurnNoticeIn,
  severTerminalNoticeIn,
  freezeActiveWork,
  hasSubagentSpawn,
  hasUntimedWork,
  holdsUserSend,
  isStopAckNotice,
  isStopCommand,
  mergeSyncPage,
  mergeWorkSteps,
  oldestRowOrdinal,
  openWorkIn,
  outlineEntries,
  reconcileWork,
  restStepToWork,
  restoreImageDims,
  rowOrdinal,
  rowsAboveFloor,
  sameTurnWorkIndex,
  sanitizeRestoredRows,
  splitForFirstPaint,
  syncSince,
  transcriptItemToRow,
  wireStepToWork,
} from "../Transcript";
import { workedLabel } from "../WorkBlock";
import type { ChatMsg, Row, TranscriptRowItem, WorkRow, WorkStep } from "../types";

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

  // The anchor is an absolute instant, so the "Worked 7h" hazard is not that it
  // was persisted — it is a LIVE block reading `now − startedAt` across the
  // app-closed gap. A CLOSED block runs neither the ticker nor the close path,
  // and stripping it there cost `segmentWorkSteps` the bounds it times runs
  // with: every restored turn collapsed to "N steps" with its true duration
  // sitting unused on the row.
  it("KEEPS startedAt on a CLOSED block — nothing there can tick it, and the runs need it", () => {
    const rows: Row[] = [work({ id: "w1", steps: [{ kind: "status", text: "reading" }], startedAt: 1, elapsedMs: 5_000 })];
    const [out] = sanitizeRestoredRows(rows) as WorkRow[];
    expect(out.startedAt).toBe(1);
    expect(out.elapsedMs).toBe(5_000);
  });

  it("keeps a block that was live at persist LIVE, and STRIPS its anchor (the 'Worked 7h' case)", () => {
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

  it("does NOT weld an adjacency the fold guard deliberately left standing", () => {
    // Two complete turns, persisted as two cards because `sameContinuingTurn`
    // refused them at the sync seam. The legacy heal must not undo that on the
    // next cold open — or the guard would hold for exactly one session.
    const rows: Row[] = [
      work({ id: "w1", steps: [{ kind: "reasoning", text: "a" }], turnComplete: true }),
      work({ id: "w2", steps: [{ kind: "reasoning", text: "b" }], turnComplete: true }),
    ];
    expect(sanitizeRestoredRows(rows)).toHaveLength(2);
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
    expect(transcriptItemToRow(item)).toEqual({ id: "pm-1", role: "user", ordinal: 4, content: "hi", attachments: undefined });
  });

  // The id it is KEYED by carries no ordinal, so the row must carry one itself —
  // `syncSince` is blind to every user message otherwise.
  it("carries the server ordinal beside the platform_msg_id key", () => {
    const item: TranscriptRowItem = { id: "m4", ordinal: 4, kind: "message", role: "user", text: "hi", platform_msg_id: "pm-1" };
    expect(transcriptItemToRow(item)).toMatchObject({ ordinal: 4 });
  });

  it("keys an assistant row by the stable m<ordinal> id and carries its attachments", () => {
    const attachments = [{ kind: "image" as const, blob_id: "sha256:ab.tok", mime_type: "image/png", size: 12 }];
    const item: TranscriptRowItem = { id: "m5", ordinal: 5, kind: "message", role: "assistant", text: "there", attachments };
    expect(transcriptItemToRow(item)).toEqual({ id: "m5", role: "assistant", ordinal: 5, content: "there", attachments });
  });

  it("carries the server's created_at — the clock under a reconstructed bubble", () => {
    const item: TranscriptRowItem = {
      id: "m9",
      ordinal: 9,
      kind: "message",
      role: "assistant",
      text: "there",
      created_at: "2026-07-23T10:00:00.000Z",
    };
    expect(transcriptItemToRow(item)).toMatchObject({ createdAt: "2026-07-23T10:00:00.000Z" });
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

  it("reloads a notice at the severity the live frame carried, not neutral", () => {
    const item: TranscriptRowItem = { id: "n5", kind: "notice", text: "skill failed", notice_level: "error" };
    expect(transcriptItemToRow(item)).toMatchObject({ level: "error" });
  });

  it("anchors a work row to the SERVER's turn start and duration", () => {
    const item: TranscriptRowItem = {
      id: "w7",
      ordinal: 7,
      kind: "work",
      steps: [{ kind: "reasoning", text: "hmm" }],
      work_started_at: "2026-07-12T10:00:00.000Z",
      work_ended_at: "2026-07-12T10:00:12.000Z",
      turn_complete: true,
    };
    expect(transcriptItemToRow(item)).toEqual({
      id: "w7",
      role: "work",
      steps: [{ kind: "reasoning", text: "hmm" }],
      active: false,
      turnComplete: true,
      startedAt: Date.parse("2026-07-12T10:00:00.000Z"),
      elapsedMs: 12_000,
    });
  });

  it("marks a /stop'd turn's block cancelled so its card says so instead of reading as a normal turn", () => {
    const item: TranscriptRowItem = { id: "w9", ordinal: 9, kind: "work", steps: [{ kind: "prose", text: "x" }], cancelled: true };
    expect(transcriptItemToRow(item)).toMatchObject({ cancelled: true });
  });

  it("leaves an unfinished work row untimed rather than inventing a duration", () => {
    const item: TranscriptRowItem = { id: "w8", kind: "work", steps: [{ kind: "prose", text: "x" }], work_started_at: "2026-07-12T10:00:00.000Z", turn_complete: false };
    // The cut-off flag rides along — it is what lets the next page's half join.
    expect(transcriptItemToRow(item)).toMatchObject({ elapsedMs: undefined, turnComplete: false });
  });
});

describe("wireStepToWork — the subscribe_state shape", () => {
  it("KEEPS call_id: without it every live tool step in a resumed turn hangs at 'running' forever", () => {
    expect(wireStepToWork({ kind: "tool", call_id: "call-9", tool: "Bash", label: "Bash(ls)" })).toEqual({
      kind: "tool",
      callId: "call-9",
      tool: "Bash",
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
      tool: "Bash",
      label: "Bash(ls)",
      status: "error",
      summary: "exit 1",
      approval: undefined,
    });
  });

  it("tolerates a row persisted before the gateway sent call_id", () => {
    expect(restStepToWork({ kind: "tool", tool: "Bash" })).toMatchObject({ callId: "" });
  });

  // A persisted result that carried no status is not evidence of success; the
  // old "ok" default painted such a step green while app/web left it neutral.
  it("leaves a statusless step NEUTRAL rather than calling it ok", () => {
    expect(restStepToWork({ kind: "tool", tool: "Read" })).toMatchObject({ label: "Read", status: "" });
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

  // Narration used to key on its TEXT alone, so a model that said the same short
  // thing twice in one turn lost the second copy to the merge. Invisible while
  // prose was hidden inside the collapse; a silently DELETED paragraph now that
  // `segmentWorkSteps` renders it. The key anchors on the tool call a paragraph
  // precedes — a row's Text and its ToolUse are ONE persisted row, so no page
  // tear can separate them and every leg agrees on the anchor.
  describe("repeated narration survives", () => {
    const SAME = "我看下测试。";
    const say = (text: string): WorkStep => ({ kind: "prose", text });
    const shape = (steps: WorkStep[]) =>
      steps.map((s) => (s.kind === "tool" ? `[${s.callId}]` : s.kind === "prose" ? s.text : `<${s.text}>`));

    it("keeps BOTH copies when a page tear puts one in each half", () => {
      const a: WorkStep[] = [say(SAME), tool({ callId: "c1" })];
      const b: WorkStep[] = [say(SAME), tool({ callId: "c2" })];
      expect(shape(mergeWorkSteps(a, b))).toEqual([SAME, "[c1]", SAME, "[c2]"]);
    });

    it("keeps both when the live half saw only the first and the server sent both", () => {
      const live: WorkStep[] = [say(SAME), tool({ callId: "c1" })];
      const recon: WorkStep[] = [say(SAME), tool({ callId: "c1" }), say(SAME), tool({ callId: "c2" })];
      expect(shape(mergeWorkSteps(live, recon))).toEqual([SAME, "[c1]", SAME, "[c2]"]);
    });

    it("does NOT double a paragraph one side holds UNANCHORED (its tool call is still in flight)", () => {
      const live: WorkStep[] = [tool({ callId: "c1" }), say(SAME)];
      const recon: WorkStep[] = [tool({ callId: "c1" }), say(SAME), tool({ callId: "c2" })];
      expect(shape(mergeWorkSteps(live, recon))).toEqual(["[c1]", SAME, "[c2]"]);
      // …and in the other direction, whichever side the merge is given first.
      expect(shape(mergeWorkSteps(recon, live))).toEqual(["[c1]", SAME, "[c2]"]);
    });

    // Found by an adversarial probe against the first version of this key: a's
    // single copy satisfied BOTH the anchored and the unanchored occurrence in
    // b, so the tail paragraph vanished. One a-copy may now be consumed once.
    it("keeps the tail paragraph when a's only copy already matched an earlier one", () => {
      const live: WorkStep[] = [tool({ callId: "c1" }), say(SAME)];
      const recon: WorkStep[] = [tool({ callId: "c1" }), say(SAME), tool({ callId: "c2" }), say(SAME)];
      expect(shape(mergeWorkSteps(live, recon))).toEqual(["[c1]", SAME, "[c2]", SAME]);
    });

    // …and the mirror trap: once b has contributed a step of its own, its
    // unanchored tail is a LATER paragraph, not a's — matching it would delete
    // one. (A page-torn turn whose halves each repeat the same sentence.)
    it("keeps a repeat that b reached only after adding its own steps", () => {
      const first: WorkStep[] = [say("甲"), tool({ callId: "c1" })];
      const second: WorkStep[] = [say("甲"), tool({ callId: "c2" }), say("甲")];
      expect(shape(mergeWorkSteps(first, second))).toEqual(["甲", "[c1]", "甲", "[c2]", "甲"]);
    });

    it("still collapses a genuinely redelivered span", () => {
      const span: WorkStep[] = [say("甲"), tool({ callId: "c1" }), say("乙"), tool({ callId: "c2" })];
      expect(shape(mergeWorkSteps(span, [...span]))).toEqual(["甲", "[c1]", "乙", "[c2]"]);
    });

    it("keeps two DIFFERENT paragraphs apart", () => {
      const a: WorkStep[] = [say("甲"), tool({ callId: "c1" })];
      const b: WorkStep[] = [say("乙"), tool({ callId: "c2" })];
      expect(shape(mergeWorkSteps(a, b))).toEqual(["甲", "[c1]", "乙", "[c2]"]);
    });
  });
});

// The REST plane folds the live channel's in-flight buffer into the trailing
// work block, and an `AnswerDelta` lands there as a `prose` step — so a BASELINE
// sync taken mid-answer returns a page whose trailing block ends with the very
// text `streamingText` is painting below it. Hidden inside the collapse until
// `segmentWorkSteps` started rendering narration; a visible duplicate after.
describe("dropInFlightAnswerStep — the page's trailing prose is the live reply", () => {
  const work = (steps: WorkStep[]): WorkRow => ({ id: "w4", role: "work", steps, active: false });
  const said = (text: string): WorkStep => ({ kind: "prose", text });

  it("strips the in-flight answer, keeping the machinery", () => {
    const out = dropInFlightAnswerStep([work([tool({ callId: "c1" }), said("答案前半段")])]);
    expect((out[0] as WorkRow).steps.map((s) => s.kind)).toEqual(["tool"]);
  });

  it("drops a block that held nothing but the answer (tool-free turn)", () => {
    expect(dropInFlightAnswerStep([work([said("你好，我是")])])).toEqual([]);
  });

  it("keeps genuine narration — a persisted prose step is never a block's last", () => {
    const out = dropInFlightAnswerStep([work([said("我先看看配置"), tool({ callId: "c1" })])]);
    expect((out[0] as WorkRow).steps.map((s) => s.kind)).toEqual(["prose", "tool"]);
  });

  it("finds the block behind a trailing notice row", () => {
    const notice = { id: "n1", role: "notice", level: "info", content: "x" } as unknown as Row;
    const out = dropInFlightAnswerStep([work([tool({ callId: "c1" }), said("答案")]), notice]);
    expect((out[0] as WorkRow).steps.map((s) => s.kind)).toEqual(["tool"]);
    expect(out).toHaveLength(2);
  });

  it("is a no-op when the page does not end in a work block", () => {
    const rows = [work([tool({ callId: "c1" })])];
    expect(dropInFlightAnswerStep(rows)).toBe(rows);
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

  it("finds a broken block anywhere in the page the baseline will re-deliver", () => {
    const rows: Row[] = [
      work({ id: "w3", steps: [{ kind: "status", text: "reading" }] }),
      { id: "m91", role: "assistant", content: "done" },
      work({ id: "w95", steps: [tool()], elapsedMs: 4_000 }),
    ];
    expect(hasUntimedWork(rows)).toBe(true);
  });

  // The window is load-bearing, not a shortcut. A non-repair REPLACE now KEEPS
  // the rows above the page's floor, so a block older than the page would
  // survive it, match again on the next open, and demote the cursor forever —
  // a baseline REPLACE every single open, for a block no baseline can reach.
  it("ignores one older than that page — nothing the baseline returns can re-time it", () => {
    const rows: Row[] = [work({ id: "w3", steps: [{ kind: "status", text: "reading" }] })];
    for (let i = 0; i < 60; i++) rows.push({ id: `m${100 + i}`, role: "assistant", content: "x" });
    expect(hasUntimedWork(rows)).toBe(false);
  });

  it("still flags one that sits just inside the window", () => {
    const rows: Row[] = [work({ id: "w3", steps: [{ kind: "status", text: "reading" }] })];
    for (let i = 0; i < 40; i++) rows.push({ id: `m${100 + i}`, role: "assistant", content: "x" });
    expect(hasUntimedWork(rows)).toBe(true);
  });
});

describe("openWorkIn / foldMidTurnNoticeIn — one turn, one card", () => {
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

  it("folds a mid_turn aside INTO an active block rather than severing it", () => {
    let rows: Row[] = openWorkIn([], (w) => ({ ...w, steps: [tool({ callId: "c1" })] }));
    rows = foldMidTurnNoticeIn(rows, "warn", "degraded");
    rows = openWorkIn(rows, (w) => ({ ...w, steps: [...w.steps, tool({ callId: "c2" })] }));

    expect(steps(rows)).toHaveLength(1);
    expect(steps(rows)[0].steps).toMatchObject([{ kind: "tool" }, { kind: "notice", text: "degraded" }, { kind: "tool" }]);
  });

  it("keeps ONE card when a mid_turn aside lands on a FROZEN block", () => {
    // The straggler sequence of the first case, with an aside in between. It
    // sees `active:false` so it keeps its own row — but that row breaks the
    // ADJACENCY `openWorkIn`'s fold-into-frozen-tail invariant rests on, so
    // the straggler would fork a SECOND card without the back-scan.
    // Wanted: [work][notice] with the straggler folded back into the one block.
    let rows: Row[] = openWorkIn([], (w) => ({ ...w, steps: [tool({ callId: "c1" })] }));
    rows = freezeActiveWork(rows);
    rows = foldMidTurnNoticeIn(rows, "warn", "degraded");
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
    rows = foldMidTurnNoticeIn(rows, "info", "compacted");
    rows = openWorkIn(rows, (w) => ({ ...w, steps: [tool({ callId: "c2" })] }));

    expect(rows.map((r) => r.role)).toEqual(["work", "assistant", "notice", "work"]);
    expect(steps(rows)[0].steps).toHaveLength(1);
  });

  it("severTerminalNoticeIn freezes the active block and keeps the notice a visible row", () => {
    // The turn-failed / blank-reply notices carry no `mid_turn` and beat the
    // projector's turn_state{inactive} — folding them would bury the turn's
    // only output inside the collapsing card.
    let rows: Row[] = openWorkIn([], (w) => ({ ...w, steps: [tool({ callId: "c1" })] }));
    rows = severTerminalNoticeIn(rows, "The turn failed before producing a reply: boom", null);

    expect(rows.map((r) => r.role)).toEqual(["work", "notice"]);
    expect(steps(rows)[0].active).toBe(false); // frozen, not left "working"
    expect(steps(rows)[0].steps).toMatchObject([{ kind: "tool" }]); // nothing folded
    const notice = rows[1];
    expect(notice.role === "notice" && notice.content).toBe(
      "The turn failed before producing a reply: boom",
    );
  });

  it("severTerminalNoticeIn keys a PERSISTED notice by its durable n<seq> id", () => {
    // The synced twin arrives as the same id, so applySyncPage's byId dedup
    // skips it — a uid-keyed copy would double-render the same text.
    const rows = severTerminalNoticeIn([], "Context compacted.", "n7");
    expect(rows).toHaveLength(1);
    expect(rows[0]).toMatchObject({ id: "n7", role: "notice", content: "Context compacted." });
  });

  it("severTerminalNoticeIn skips minting when the durable row is already on screen", () => {
    const existing: Row[] = [
      openWorkIn([], (w) => ({ ...w, steps: [tool({ callId: "c1" })] }))[0],
      { id: "n7", role: "notice", content: "Context compacted." },
    ];
    const rows = severTerminalNoticeIn(existing, "Context compacted.", "n7");
    expect(rows.filter((r) => r.role === "notice")).toHaveLength(1); // no double
    expect(steps(rows)[0].active).toBe(false); // still freezes
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
  // `turnComplete` is the server's `turn_complete`: the older page's half was
  // cut off by that window's trailing edge (`false` — its turn continues into
  // the next page), the newer page's half was closed by the answer (`true`).
  const first = (): WorkRow =>
    work({ id: "w3", steps: [tool({ callId: "", label: "Fetch(a)" })], startedAt: TURN_START, elapsedMs: FIRST_END - TURN_START, turnComplete: false });
  const second = (): WorkRow =>
    work({ id: "w43", steps: [tool({ callId: "", label: "Fetch(b)" })], startedAt: SECOND_START, elapsedMs: TURN_END - SECOND_START, turnComplete: true });

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
    // Two cut-off halves, then the one the answer closed: the fused pair stays
    // cut off (it takes the NEWER half's completeness), so the chain continues.
    const middle = work({ id: "w43", steps: [tool({ callId: "", label: "Fetch(b)" })], startedAt: SECOND_START, elapsedMs: 1_000, turnComplete: false });
    const third = work({ id: "w80", steps: [tool({ callId: "", label: "Fetch(c)" })], startedAt: SECOND_START, elapsedMs: TURN_END - SECOND_START, turnComplete: true });
    const out = foldAdjacentWork([first(), middle, third]) as WorkRow[];
    expect(out).toHaveLength(1);
    expect(out[0]).toMatchObject({ id: "w3", startedAt: TURN_START, elapsedMs: TURN_END - TURN_START, turnComplete: true });
    expect(out[0].steps).toHaveLength(3);
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

  it("does NOT fuse two halves across a compaction boundary (they are distinct turns)", () => {
    // A mid-turn compaction split w3 | w43 at watermark 20 (server-side). Without
    // the guard iOS's fold would re-join them into one card and swallow the
    // divider; with the boundary between their ordinals they stay two cards.
    //
    // This is also the case `turn_complete` does NOT subsume, which is why both
    // guards stay: the head here is an ordinary cut-off (`false`) block — a
    // watermark landing in the GAP between two pages is straddled by no single
    // reconstruction window, so neither page splits its own half and only the
    // compaction guard knows the seam is there.
    const out = foldAdjacentWork([first(), second()], [{ ordinal: 20, at: "t" }]);
    expect(out).toHaveLength(2);
    expect((out as WorkRow[]).map((r) => r.id)).toEqual(["w3", "w43"]);
    // A boundary OUTSIDE the pair (below both) still lets them fuse.
    expect(foldAdjacentWork([first(), second()], [{ ordinal: 2, at: "t" }])).toHaveLength(1);
  });

  it("does NOT fuse a COMPLETE block with the block after it (two turns, two cards)", () => {
    // The scar: a sync bug put blocks from three different turns side by side
    // and adjacency alone welded them into one "Worked 2h 47m" card carrying
    // every turn's steps. The same shape arises without any bug — a turn whose
    // empty final reply left no bubble, abutting the next fire — so the server's
    // word is the only thing that tells the two apart.
    const out = foldAdjacentWork([{ ...first(), turnComplete: true }, second()]) as WorkRow[];
    expect(out).toHaveLength(2);
    expect(out.map((r) => r.id)).toEqual(["w3", "w43"]);
    expect(out.map((r) => r.steps.length)).toEqual([1, 1]); // no card wears another turn's steps
  });

  it("DECLINES when completeness is unknown — a mirror written before the flag existed", () => {
    // Refusing costs one extra card; joining wrongly swallows a whole turn.
    const out = foldAdjacentWork([{ ...first(), turnComplete: undefined }, second()]);
    expect(out).toHaveLength(2);
  });

  // The server flags the half its `/stop` closed and resets per flush, so only
  // the NEWER half of a page-torn turn ever carries it — the joined card must
  // still read "Cancelled".
  it("keeps the newer half's cancelled flag on the joined card", () => {
    const [row] = foldAdjacentWork([first(), { ...second(), cancelled: true }]) as [WorkRow];
    expect(row).toMatchObject({ id: "w3", cancelled: true });
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
      // Neither side claims a cancel, and a reconciled card always STATES that:
      // the flag is only ever known server-side, so "no news" is settled news.
      cancelled: false,
      startedAt: 100,
      elapsedMs: 8_000,
      steps: [tool({ callId: "c1" }), { kind: "reasoning", text: "r" }],
    });
  });

  it("falls back to the live anchors when the server carries none", () => {
    const base = work({ id: "live", steps: [], active: true, startedAt: 999, elapsedMs: 5 });
    expect(reconcileWork(base, work({ id: "w1" }))).toMatchObject({ startedAt: 999, elapsedMs: 5 });
  });

  // The ONLY way a `/stop`ped block gets its label: the live card knows nothing
  // about the cancel — the reconstruction carries the flag.
  it("takes the server's cancelled flag onto the live block", () => {
    const base = work({ id: "live", steps: [tool({ callId: "c1" })], active: true });
    expect(reconcileWork(base, work({ id: "w7", cancelled: true }))).toMatchObject({ cancelled: true });
  });

  it("never un-cancels a card the base already carried", () => {
    const base = work({ id: "live", steps: [], cancelled: true });
    expect(reconcileWork(base, work({ id: "w7", cancelled: false }))).toMatchObject({ cancelled: true });
  });
});

describe("a cancelled turn's card says so", () => {
  /// The label helpers own the KEY plus its interpolation, not the English copy
  /// (`formatters.test.ts` uses the same stand-in).
  const t = ((key: string, values?: unknown) =>
    values === undefined ? key : `${key}:${JSON.stringify(values)}`) as unknown as TFunction;

  it("carries `cancelled` from the wire row all the way to the closed card's label", () => {
    const item: TranscriptRowItem = {
      id: "w9",
      ordinal: 9,
      kind: "work",
      steps: [{ kind: "prose", text: "half an answer" }],
      work_started_at: "2026-07-12T10:00:00.000Z",
      work_ended_at: "2026-07-12T10:00:07.000Z",
      cancelled: true,
    };
    const row = transcriptItemToRow(item) as WorkRow;
    expect(workedLabel(t, row.elapsedMs, row.cancelled)).toContain("chat.cancelledFor");
  });

  it("says just 'Cancelled' when the turn was too short to time", () => {
    expect(workedLabel(t, 400, true)).toBe("chat.cancelled");
    expect(workedLabel(t, undefined, true)).toBe("chat.cancelled");
  });

  it("leaves an ordinary completed turn's label alone", () => {
    expect(workedLabel(t, 400, false)).toBe("chat.worked");
    expect(workedLabel(t, 7_000)).toContain("chat.workedFor");
  });
});

describe("sameTurnWorkIndex", () => {
  const stray = (id: string, over: Partial<WorkRow> = {}): WorkRow =>
    work({ id, steps: [{ kind: "status", text: "reading" }], ...over });

  it("finds the turn's block above its answer bubble", () => {
    const rows: Row[] = [work({ id: "w9", steps: [tool()] }), { id: "m12", role: "assistant", content: "a" }];
    expect(sameTurnWorkIndex(rows, stray("w10"))).toBe(0);
  });

  it("scans back over trailing notices too", () => {
    const rows: Row[] = [
      work({ id: "w9", steps: [tool()] }),
      { id: "m12", role: "assistant", content: "a" },
      { id: "n1", role: "notice", content: "note" },
    ];
    expect(sameTurnWorkIndex(rows, stray("w10"))).toBe(0);
  });

  it("refuses when the trailing answer is OLDER than the block (a genuinely later turn)", () => {
    const rows: Row[] = [work({ id: "w9", steps: [tool()] }), { id: "m5", role: "assistant", content: "a" }];
    expect(sameTurnWorkIndex(rows, stray("w10"))).toBe(-1);
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
    expect(sameTurnWorkIndex(rows, stray("w10"))).toBe(-1);
  });

  // A tool-free "thinking only" turn has no intermediate row to key off, so the
  // gateway seeds its block from the ANSWER ROW itself — `w12` and `m12` are one
  // turn. The strict `>` this used to demand is unsatisfiable there, which is
  // how a whole class of turns kept re-homing to nothing and stranding a second
  // "Worked" card under the reply.
  it("accepts the answer whose ordinal the block SHARES, when told the block owns it", () => {
    const rows: Row[] = [work({ id: "u-live", steps: [tool()] }), { id: "m12", role: "assistant", content: "a" }];
    expect(sameTurnWorkIndex(rows, stray("w12"), true)).toBe(0);
  });

  // …and refuses it without that evidence. An ordinal can also be BORROWED — a
  // block seeded from a progress event anchored at the row above it — and there
  // the same equality means the opposite. Folding on it would weld two turns.
  it("refuses a shared ordinal it was given no evidence for", () => {
    const rows: Row[] = [work({ id: "u-live", steps: [tool()] }), { id: "m12", role: "assistant", content: "a" }];
    expect(sameTurnWorkIndex(rows, stray("w12"))).toBe(-1);
  });
});

describe("holdsUserSend — the outbox re-seed runs on every mount, so it must be idempotent", () => {
  const queued = (id: string): Row => ({ id, role: "user", content: "hi", sendState: "sending" });

  it("recognises a bubble the restored mirror already carries", () => {
    expect(holdsUserSend([queued("pm-1")], "pm-1")).toBe(true);
  });

  // The resync case: the mirror is gone, so the rebuilt page has nothing and
  // every unconfirmed send has to come back.
  it("says no on an empty thread", () => {
    expect(holdsUserSend([], "pm-1")).toBe(false);
  });

  // The mirror predating the send (a jetsam before the debounced write) is the
  // pre-existing hole this closes — the thread is there, the bubble is not.
  it("says no when the thread predates the send", () => {
    expect(holdsUserSend([{ id: "m10", role: "assistant", content: "older" }], "pm-1")).toBe(false);
  });

  // A send is keyed by its `platform_msg_id` and an assistant row by `m<ordinal>`,
  // so the roles cannot collide — but the check must not rely on that.
  it("ignores a non-user row that happens to share the id", () => {
    expect(holdsUserSend([{ id: "pm-1", role: "assistant", content: "hi" }], "pm-1")).toBe(false);
  });

  // The bubble stays keyed by its `platform_msg_id` once the echo confirms it,
  // and its outbox entry lives on until DURABILITY does — so a re-seed in that
  // window must still recognise it.
  it("recognises a bubble whose echo already cleared the spinner", () => {
    expect(holdsUserSend([{ id: "pm-1", role: "user", content: "hi", ordinal: 7 }], "pm-1")).toBe(true);
  });
});

describe("syncSince / mergeSyncPage — a difference page must EXTEND the thread, never overlap it", () => {
  // The scar: a device mirror rendering rows far above its persisted cursor (a
  // COVERAGE watermark, which scroll-up paging and a rebase-dirty freeze never
  // advance) asked for the difference anyway. The server answered correctly —
  // as a difference, not a rebase, so nothing self-healed — and the merge welded
  // three hours of conversation 75 rows up the thread and fused three turns into
  // one "Worked 2h 47m" card.
  const rendered = (): Row[] => [
    { id: "m10", role: "assistant", content: "older" },
    { id: "m20", role: "assistant", content: "newer" },
    work({ id: "w30", steps: [tool({ callId: "c30" })], startedAt: 30_000, elapsedMs: 1_000 }),
  ];
  // What the gateway answers: rows strictly above `since`, or the newest page
  // (a REPLACE) when the client presents no cursor at all.
  const answer = (since: number | null): { rows: Row[]; replace: boolean } => {
    const all: Row[] = [
      { id: "m10", role: "assistant", content: "older" },
      work({ id: "w15", steps: [tool({ callId: "c15" })], startedAt: 15_000, elapsedMs: 1_000 }),
      { id: "m18", role: "assistant", content: "missing" },
      { id: "m20", role: "assistant", content: "newer" },
      work({ id: "w30", steps: [tool({ callId: "c30" })], startedAt: 30_000, elapsedMs: 1_000 }),
    ];
    return since === null
      ? { rows: all, replace: true }
      : { rows: all.filter((r) => (rowOrdinal(r.id) ?? -1) > since), replace: false };
  };
  // The one client loop (docs/sync-protocol.md), exactly as `runSync` +
  // `applySyncPage` run it: ask with `syncSince`, REPLACE a baseline, merge a
  // difference.
  const syncOnce = (rows: Row[], cursor: number | null): Row[] => {
    const page = answer(syncSince(cursor, rows));
    return page.replace ? page.rows : mergeSyncPage(rows, page.rows, []);
  };

  it("rebuilds a thread the cursor does not cover, in ordinal order", () => {
    // Unguarded this asks since=5, and the merge appends m18 BELOW w30 while
    // folding w15 into w30's card: ["m10", "m20", "w30", "m18"].
    expect(syncOnce(rendered(), 5).map((r) => r.id)).toEqual(["m10", "w15", "m18", "m20", "w30"]);
  });

  it("leaves every work card its own turn — no card carries two turns' steps", () => {
    const blocks = syncOnce(rendered(), 5).filter((r): r is WorkRow => r.role === "work");
    expect(blocks.map((r) => r.steps.length)).toEqual([1, 1]);
  });

  it("still differences an ordinary page — the cursor covers the thread, so nothing changes", () => {
    const rows: Row[] = [
      { id: "m10", role: "assistant", content: "a" },
      { id: "m20", role: "assistant", content: "b" },
    ];
    expect(syncSince(20, rows)).toBe(20);
    const page: Row[] = [
      { id: "m21", role: "user", content: "next" },
      work({ id: "w25", steps: [tool({ callId: "c25" })] }),
      { id: "m30", role: "assistant", content: "reply" },
    ];
    expect(mergeSyncPage(rows, page, []).map((r) => r.id)).toEqual(["m10", "m20", "m21", "w25", "m30"]);
  });

  // The hole the byte-mirror had: iOS keys a user row by its `platform_msg_id`,
  // so `rowOrdinal` answers `null` for EVERY user message — while app/web keeps
  // `row-<sid>-m<ordinal>` and counts them. A rebase-dirty freeze renders another
  // device's sends above the cursor and nothing else, and the guard would present
  // the cursor anyway — the exact welding merge it exists to prevent.
  it("presents the baseline for another device's user row above the cursor", () => {
    const rows: Row[] = [
      { id: "m10", role: "assistant", content: "a" },
      { id: "pm-laptop-1", role: "user", ordinal: 21, content: "sent from my laptop" },
    ];
    expect(syncSince(10, rows)).toBeNull();
  });

  it("counts a send of ours once its durable row confirms it — the id never gains an ordinal", () => {
    const optimistic: Row[] = [{ id: "pm-1", role: "user", content: "hi", sendState: "sending" }];
    expect(syncSince(10, optimistic)).toBe(10); // unconfirmed: not durable coverage
    const confirmed = mergeSyncPage(optimistic, [{ id: "pm-1", role: "user", ordinal: 11, content: "hi" }], []);
    expect(confirmed[0]).toMatchObject({ ordinal: 11, sendState: undefined });
    expect(syncSince(10, confirmed)).toBeNull(); // a cursor left behind it
    expect(syncSince(11, confirmed)).toBe(11); // and quiet again once covered
  });

  // A terminal notice is minted live under its `durable_id`, so the redelivered
  // durable twin dedups by id — and is the only thing that ever knows its level.
  it("upgrades a live-minted notice to the severity its durable twin carries", () => {
    const prev: Row[] = [{ id: "n7", role: "notice", content: "skill failed" }];
    const merged = mergeSyncPage(prev, [{ id: "n7", role: "notice", content: "skill failed", level: "error" }], []);
    expect(merged).toHaveLength(1);
    expect(merged[0]).toMatchObject({ id: "n7", level: "error" });
  });

  it("is quiet on rows carrying no ordinal — an optimistic send is not durable coverage", () => {
    const rows: Row[] = [
      { id: "m10", role: "assistant", content: "a" },
      { id: "pm-uuid", role: "user", content: "just sent", sendState: "sending" },
      work({ id: "u7-abc", steps: [tool()], active: true }),
      { id: "n4", role: "notice", content: "note" },
    ];
    expect(syncSince(10, rows)).toBe(10);
  });

  it("is quiet when the cursor sits exactly ON the newest rendered row", () => {
    expect(syncSince(30, rendered())).toBe(30);
  });

  it("stays null with no cursor at all (the fresh-install baseline)", () => {
    expect(syncSince(null, rendered())).toBeNull();
  });

  it("never fuses across a compaction boundary at the merge seam", () => {
    const prev: Row[] = [
      { id: "m10", role: "assistant", content: "a" },
      work({ id: "w20", steps: [tool({ callId: "c20" })], turnComplete: false }),
    ];
    const page: Row[] = [work({ id: "w40", steps: [tool({ callId: "c40" })], turnComplete: true })];
    expect(mergeSyncPage(prev, page, [{ ordinal: 30, at: "t" }]).map((r) => r.id)).toEqual([
      "m10",
      "w20",
      "w40",
    ]);
    // No boundary between them ⇒ still one turn cut by the page edge, still one card.
    expect(mergeSyncPage(prev, page, [])).toHaveLength(2);
  });

  it("never fuses a COMPLETE tail with the next turn's block at the merge seam", () => {
    const prev: Row[] = [
      { id: "m10", role: "assistant", content: "a" },
      work({ id: "w20", steps: [tool({ callId: "c20" })], turnComplete: true }),
    ];
    const page: Row[] = [work({ id: "w40", steps: [tool({ callId: "c40" })], turnComplete: true })];
    const out = mergeSyncPage(prev, page, []) as [ChatMsg, WorkRow, WorkRow];
    expect(out.map((r) => r.id)).toEqual(["m10", "w20", "w40"]);
    expect(out[1].steps).toHaveLength(1);
  });

  it("still reconciles a LIVE tail with its own reconstruction (it carries no flag)", () => {
    const prev: Row[] = [work({ id: "u7-abc", steps: [tool({ callId: "c1" })], active: true, startedAt: 999 })];
    const page: Row[] = [work({ id: "w40", steps: [tool({ callId: "c2" })], startedAt: 40_000, turnComplete: false })];
    const out = mergeSyncPage(prev, page, []) as WorkRow[];
    expect(out).toHaveLength(1);
    expect(out[0]).toMatchObject({ id: "u7-abc", active: true, startedAt: 40_000 });
    expect(out[0].steps).toHaveLength(2);
  });

  // What the unguarded merge left on disk: the welded span below the tail, and
  // w15's step fused into w30's card.
  const scrambled = (): Row[] => [
    { id: "m10", role: "assistant", content: "older" },
    { id: "m20", role: "assistant", content: "newer" },
    work({
      id: "w30",
      steps: [tool({ callId: "c30" }), tool({ callId: "c15" })],
      startedAt: 30_000,
      elapsedMs: 1_000,
    }),
    { id: "m18", role: "assistant", content: "missing" },
  ];

  it("re-orders an already-scrambled mirror while its cursor is still behind the tail", () => {
    expect(syncOnce(scrambled(), 5).map((r) => r.id)).toEqual(["m10", "w15", "m18", "m20", "w30"]);
  });

  // The guard PREVENTS the scramble; it does not heal every mirror already
  // holding one. A difference that returns at all scanned to the end of the log
  // (`sync_difference` rebases instead once the scan overruns), so the very
  // response that welded the span carries a `next_cursor` at the session's
  // NEWEST ordinal — above every rendered row, leaving the guard quiet on the
  // next open. That mirror needs the resync hatch, not another sync.
  it("cannot re-order one whose cursor that same sync already advanced past the tail", () => {
    expect(syncOnce(scrambled(), 30).map((r) => r.id)).toEqual(["m10", "m20", "w30", "m18"]);
  });
});

describe("mergeSyncPage — a page that lands late is placed, not piled on the end", () => {
  // `syncSince`'s prefix gate runs when the request is POSTED, and the thread
  // keeps growing during the round trip. A difference asked for at `since` can
  // land after live frames rendered rows above it — and a blind tail append then
  // files the page's rows below rows they predate. Nothing is lost; the order is
  // wrong, it persists into the mirror, and the gate stays quiet over it after.
  it("files a late page row at its ordinal, under the rows it predates", () => {
    // Asked at since=100; while it flew, the turn rendered live through m103.
    const rendered: Row[] = [
      { id: "m100", role: "assistant", ordinal: 100, content: "older" },
      { id: "m103", role: "assistant", ordinal: 103, content: "the reply" },
    ];
    const page: Row[] = [
      { id: "pm-x", role: "user", ordinal: 101, content: "asked" },
      work({ id: "w102", steps: [tool({ callId: "c1" })] }),
    ];
    expect(mergeSyncPage(rendered, page, []).map((r) => r.id)).toEqual(["m100", "pm-x", "w102", "m103"]);
  });

  // The reported `[work][reply][work]` shape, difference edition. A tool-free
  // "thinking only" turn has no intermediate row, so the gateway seeds its block
  // from the ANSWER ROW: `w12` and `m12` are one turn, and the page proves it by
  // emitting the block above the bubble. The reply is already on screen (it
  // arrived as a live frame), so the page's block has to re-home into the live
  // card rather than pile up under the answer it belongs to.
  it("re-homes a block that shares its own answer's ordinal into the live card", () => {
    const rendered: Row[] = [
      { id: "pm-1", role: "user", content: "explain" },
      work({ id: "u-live", steps: [{ kind: "reasoning", text: "weighing" }], active: true }),
      { id: "m12", role: "assistant", ordinal: 12, content: "the reply" },
    ];
    const page: Row[] = [
      work({ id: "w12", steps: [{ kind: "reasoning", text: "weighing" }], turnComplete: true, elapsedMs: 3_000 }),
      { id: "m12", role: "assistant", ordinal: 12, content: "the reply" },
    ];
    const out = mergeSyncPage(rendered, page, []);
    expect(out.map((r) => r.id)).toEqual(["pm-1", "u-live", "m12"]);
    expect(out[1]).toMatchObject({ elapsedMs: 3_000 });
  });

  // Same turn, but nothing on screen to re-home INTO (the reasoning never
  // streamed here — another device's turn, or a reconnect that missed it). Then
  // placement decides, and "just past the last row I am not older than" is the
  // wrong answer twice over: it files the card below its own reply, and with the
  // reply's twin ordinal excluded it climbs above the QUESTION instead. Anchor on
  // the bubble.
  it("files a block that owns its answer's ordinal directly above that answer", () => {
    const rendered: Row[] = [
      { id: "m8", role: "assistant", ordinal: 8, content: "older" },
      { id: "pm-1", role: "user", content: "explain" },
      { id: "m12", role: "assistant", ordinal: 12, content: "the reply" },
    ];
    const page: Row[] = [
      work({ id: "w12", steps: [{ kind: "reasoning", text: "weighing" }], turnComplete: true }),
      { id: "m12", role: "assistant", ordinal: 12, content: "the reply" },
    ];
    expect(mergeSyncPage(rendered, page, []).map((r) => r.id)).toEqual(["m8", "pm-1", "w12", "m12"]);
  });

  // The same equality, opposite meaning: a block whose ordinal is BORROWED from
  // the row above it (seeded off a progress event anchored there — control
  // events sort after their anchor, so the page emits the block BELOW the
  // bubble) belongs to the NEXT turn. Folding it back would weld two turns into
  // one card, which is the trade this whole rule is calibrated against.
  it("appends a block that only borrowed the ordinal of the answer above it", () => {
    const rendered: Row[] = [
      work({ id: "u-live", steps: [tool({ callId: "c1" })] }),
      { id: "m12", role: "assistant", ordinal: 12, content: "the reply" },
    ];
    const page: Row[] = [
      { id: "m12", role: "assistant", ordinal: 12, content: "the reply" },
      work({ id: "w12", steps: [{ kind: "status", text: "reading" }], turnComplete: true }),
    ];
    const out = mergeSyncPage(rendered, page, []);
    expect(out.map((r) => r.id)).toEqual(["u-live", "m12", "w12"]);
    expect((out[0] as WorkRow).steps).toHaveLength(1);
  });

  // Refuses to guess. Whether another device's durable row belongs above or
  // below your own unstamped pending bubble is not knowable here, so it appends
  // — the behaviour before placement existed. Ordering the two is a tie; the
  // case below is not, which is why the tie loses.
  it("appends rather than order a durable row against an ordinal-less trailing run", () => {
    const rendered: Row[] = [
      { id: "m100", role: "assistant", ordinal: 100, content: "older" },
      { id: "pm-mine", role: "user", content: "just sent", sendState: "sending" },
      work({ id: "u-live", steps: [tool()], active: true }),
    ];
    const page: Row[] = [{ id: "pm-laptop", role: "user", ordinal: 101, content: "from my laptop" }];
    expect(mergeSyncPage(rendered, page, []).map((r) => r.id)).toEqual([
      "m100",
      "pm-mine",
      "u-live",
      "pm-laptop",
    ]);
  });

  // The shape three review rounds kept re-breaking: a live thread's user rows
  // carry NO ordinal (the echo brings none), so a trailing run of
  // [unstamped send, live block] must not be treated as placeable — file the
  // turn's answer above its own question and the reply renders over the card
  // that produced it, and the block never freezes.
  it("does not file a turn's answer above its own unstamped question", () => {
    const rendered: Row[] = [
      { id: "m99", role: "assistant", ordinal: 99, content: "older" },
      { id: "pm-mine", role: "user", content: "the question", sendState: "sending" },
      work({ id: "u-live", steps: [tool()], active: true, startedAt: NOW - 5_000 }),
    ];
    vi.useFakeTimers();
    vi.setSystemTime(NOW);
    const page: Row[] = [{ id: "m101", role: "assistant", ordinal: 101, content: "the reply" }];
    const out = mergeSyncPage(rendered, page, []);
    expect(out.map((r) => r.id)).toEqual(["m99", "pm-mine", "u-live", "m101"]);
    expect(out[2]).toMatchObject({ active: false });
  });

  it("does not leapfrog a trailing notice — an `n<seq>` seq is not an ordinal", () => {
    const rendered: Row[] = [
      { id: "m100", role: "assistant", ordinal: 100, content: "older" },
      { id: "n7", role: "notice", content: "Stopped." },
    ];
    const page: Row[] = [{ id: "m101", role: "assistant", ordinal: 101, content: "later" }];
    expect(mergeSyncPage(rendered, page, []).map((r) => r.id)).toEqual(["m100", "n7", "m101"]);
  });

  // The answer of the turn that block is STILL RUNNING — arriving by sync
  // precisely because its live frame was missed. It outranks every ordinal on
  // screen, so it belongs at the end, and appending is what freezes the card.
  // Placing it above would render the reply over its own block and strand that
  // block "Working…" forever: `reconcileWork` keeps a folded live block's uid
  // and `active`, so nothing downstream ever makes it orderable, and the next
  // turn's steps then weld into it.
  it("appends its own turn's answer and freezes the live block — placement must not preempt the tail", () => {
    const rendered: Row[] = [
      { id: "pm-mine", role: "user", ordinal: 100, content: "asked" },
      work({ id: "u-live", steps: [tool()], active: true, startedAt: NOW - 5_000 }),
    ];
    vi.useFakeTimers();
    vi.setSystemTime(NOW);
    const page: Row[] = [{ id: "m102", role: "assistant", ordinal: 102, content: "the reply" }];
    const out = mergeSyncPage(rendered, page, []);
    expect(out.map((r) => r.id)).toEqual(["pm-mine", "u-live", "m102"]);
    expect(out[1]).toMatchObject({ active: false, elapsedMs: 5_000 });
  });

  it("does not freeze a live block it was filed above — a durable row below proves a later turn", () => {
    const rendered: Row[] = [
      { id: "m100", role: "assistant", ordinal: 100, content: "older" },
      { id: "m103", role: "assistant", ordinal: 103, content: "a later turn's reply" },
      work({ id: "u-live", steps: [tool()], active: true }),
    ];
    const page: Row[] = [{ id: "m101", role: "assistant", ordinal: 101, content: "landed late" }];
    const out = mergeSyncPage(rendered, page, []);
    expect(out.map((r) => r.id)).toEqual(["m100", "m101", "m103", "u-live"]);
    expect(out[3]).toMatchObject({ active: true });
  });

  it("still plain-appends the ordinary forward page — placement is the rare branch", () => {
    const rendered: Row[] = [{ id: "m10", role: "assistant", ordinal: 10, content: "a" }];
    const page: Row[] = [
      { id: "m11", role: "assistant", ordinal: 11, content: "b" },
      { id: "m12", role: "assistant", ordinal: 12, content: "c" },
    ];
    expect(mergeSyncPage(rendered, page, []).map((r) => r.id)).toEqual(["m10", "m11", "m12"]);
  });

  it("reconciles by id AFTER a placement shifted the indices", () => {
    // The splice invalidates every index at or past it, and the next row is
    // matched through that same map — a patched-up index would write the
    // reconciled row over the wrong slot.
    const rendered: Row[] = [
      { id: "m100", role: "assistant", ordinal: 100, content: "older" },
      { id: "m103", role: "assistant", ordinal: 103, content: "the reply" },
    ];
    const page: Row[] = [
      { id: "m101", role: "assistant", ordinal: 101, content: "placed" },
      { id: "m103", role: "assistant", ordinal: 103, content: "the reply", createdAt: "2026-08-02T10:00:00.000Z" },
    ];
    const out = mergeSyncPage(rendered, page, []);
    expect(out.map((r) => r.id)).toEqual(["m100", "m101", "m103"]);
    expect(out[2]).toMatchObject({ id: "m103", createdAt: "2026-08-02T10:00:00.000Z" });
  });

  // The documented limit, and why the apply-time prefix re-check exists: an
  // `n<seq>` notice carries a SEQUENCE number, not an ordinal (`rowOrdinal`
  // matches only `m`/`w`), so there is nothing to place it by.
  it("cannot place a notice — it appends, and `applySyncPage` refuses the page instead", () => {
    const rendered: Row[] = [
      { id: "m100", role: "assistant", ordinal: 100, content: "older" },
      { id: "m103", role: "assistant", ordinal: 103, content: "the reply" },
    ];
    const page: Row[] = [{ id: "n7", role: "notice", content: "a warning from ordinal 101" }];
    expect(mergeSyncPage(rendered, page, []).map((r) => r.id)).toEqual(["m100", "m103", "n7"]);
    // Which is exactly the condition the re-check keys on.
    expect(syncSince(100, rendered)).toBeNull();
  });

  // `placeAt` always splices directly below an ordinal-bearing row, which may be
  // a work block whose turn the placed row continues. The tail-append and
  // re-home branches ask the fold guards; the splice asked nothing, so one turn
  // could land as two adjacent cards with no row between them.
  it("folds a placed half into the block above it — one turn, one card", () => {
    const rendered: Row[] = [
      { id: "m10", role: "user", ordinal: 10, content: "asked" },
      work({ id: "w11", steps: [tool({ callId: "c1" })], turnComplete: false }),
      { id: "m13", role: "assistant", ordinal: 13, content: "the reply" },
      { id: "pm-mine", role: "user", content: "asked again", sendState: "sending" },
    ];
    const page: Row[] = [work({ id: "w12", steps: [tool({ callId: "c2" })], turnComplete: true })];
    const out = mergeSyncPage(rendered, page, []);
    expect(out.map((r) => r.id)).toEqual(["m10", "w11", "m13", "pm-mine"]);
    expect((out[1] as WorkRow).steps.map((s) => (s.kind === "tool" ? s.callId : s.kind))).toEqual([
      "c1",
      "c2",
    ]);
  });

  it("but leaves two COMPLETE blocks apart — those are two turns, and two cards is right", () => {
    const rendered: Row[] = [
      { id: "m10", role: "user", ordinal: 10, content: "asked" },
      work({ id: "w11", steps: [tool({ callId: "c1" })], turnComplete: true }),
      { id: "m13", role: "assistant", ordinal: 13, content: "the reply" },
      { id: "pm-mine", role: "user", content: "asked again", sendState: "sending" },
    ];
    const page: Row[] = [work({ id: "w12", steps: [tool({ callId: "c2" })], turnComplete: true })];
    expect(mergeSyncPage(rendered, page, []).map((r) => r.id)).toEqual([
      "m10",
      "w11",
      "w12",
      "m13",
      "pm-mine",
    ]);
  });

  it("never fuses a placed half across a compaction boundary", () => {
    const rendered: Row[] = [
      { id: "m10", role: "user", ordinal: 10, content: "asked" },
      work({ id: "w11", steps: [tool({ callId: "c1" })], turnComplete: false }),
      { id: "m13", role: "assistant", ordinal: 13, content: "the reply" },
      { id: "pm-mine", role: "user", content: "asked again", sendState: "sending" },
    ];
    const page: Row[] = [work({ id: "w12", steps: [tool({ callId: "c2" })], turnComplete: true })];
    const out = mergeSyncPage(rendered, page, [{ ordinal: 12, at: "2026-08-09T10:00:00.000Z" }]);
    expect(out.map((r) => r.id)).toEqual(["m10", "w11", "w12", "m13", "pm-mine"]);
  });
});

describe("rowsAboveFloor — a rebase must not cost the reader the history they paged in", () => {
  const none = new Set<string>();
  const msg = (ordinal: number): Row => ({
    id: `m${ordinal}`,
    role: "assistant",
    content: `r${ordinal}`,
  });

  it("keeps everything the page's window does not reach", () => {
    const rows = [msg(10), msg(11), msg(12), msg(13)];
    expect(rowsAboveFloor(rows, 12, none).map((r) => r.id)).toEqual(["m10", "m11"]);
  });

  it("keeps nothing when the page reaches below the thread's floor", () => {
    expect(rowsAboveFloor([msg(10), msg(11)], 10, none)).toEqual([]);
  });

  it("keeps the whole thread when the page starts above all of it", () => {
    expect(rowsAboveFloor([msg(10), msg(11)], 99, none).map((r) => r.id)).toEqual(["m10", "m11"]);
  });

  // The cut is by POSITION for exactly this: a notice carries no ordinal, so
  // filtering on `ordinal < floor` would delete every one of them out of the
  // half that survives.
  it("carries an ordinal-less notice along with the neighbours it sits between", () => {
    const rows: Row[] = [msg(10), { id: "n3", role: "notice", content: "stopped" }, msg(11), msg(12)];
    expect(rowsAboveFloor(rows, 12, none).map((r) => r.id)).toEqual(["m10", "n3", "m11"]);
  });

  it("leaves an ordinal-less row BELOW the cut to the page, not to the head", () => {
    const rows: Row[] = [msg(10), msg(12), { id: "n3", role: "notice", content: "stopped" }];
    expect(rowsAboveFloor(rows, 12, none).map((r) => r.id)).toEqual(["m10"]);
  });

  // A work block is keyed `w<ordinal>` and counts as coverage just like a
  // message — otherwise the cut would walk straight past a turn the page holds.
  it("counts a work block's ordinal", () => {
    const rows: Row[] = [msg(10), work({ id: "w11" }), msg(12)];
    expect(rowsAboveFloor(rows, 11, none).map((r) => r.id)).toEqual(["m10"]);
  });

  it("drops a row the rebuild already carries, so no id can render twice", () => {
    const rows: Row[] = [
      { id: "pm-1", role: "user", content: "hi" },
      msg(10),
      msg(11),
    ];
    expect(rowsAboveFloor(rows, 11, new Set(["pm-1"])).map((r) => r.id)).toEqual(["m10"]);
  });
});

describe("applySyncReplace — the overlay keeps what the page cannot carry", () => {
  // The scar: the gateway echoes an inbound message to its own sender BEFORE
  // handing it to the router that persists it, and that echo frame carries no
  // ordinal — so `markSent` cleared the spinner on a row the gateway had not
  // written yet. The overlay gated on `sendState`, so a first send whose own
  // `connEpoch` sync raced its persistence had its bubble DELETED. The answer's
  // ordinal then leapfrogged the cursor (a difference selects strictly `>`), so
  // no later sync could bring it back, and the outbox's next mount-edge replay
  // re-appended it BELOW the answer.
  const owed = (...ids: string[]) => new Set(ids);
  const echoed = (): Row[] => [{ id: "pm-1", role: "user", content: "hi", sendState: undefined }];

  it("keeps an echoed-but-unpersisted send — the echo clears the spinner, not the row", () => {
    expect(applySyncReplace(echoed(), [], owed("pm-1")).map((r) => r.id)).toEqual(["pm-1"]);
  });

  it("keeps one still spinning, and one that failed — the red dot is its only retry affordance", () => {
    const prev: Row[] = [
      { id: "pm-1", role: "user", content: "a", sendState: "sending" },
      { id: "pm-2", role: "user", content: "b", sendState: "failed" },
    ];
    expect(applySyncReplace(prev, [], owed("pm-1", "pm-2")).map((r) => r.id)).toEqual(["pm-1", "pm-2"]);
  });

  it("DROPS it once the page carries the durable twin — which renders in its place, in order", () => {
    const page: Row[] = [
      { id: "m10", role: "assistant", content: "earlier" },
      { id: "pm-1", role: "user", ordinal: 11, content: "hi" },
    ];
    // The page carrying the id is also what retires it from the owed set, so in
    // the live path this predicate is belt AND braces.
    expect(applySyncReplace(echoed(), page, owed("pm-1")).map((r) => r.id)).toEqual(["m10", "pm-1"]);
  });

  // The regression the FIRST attempt at this fix shipped past its own tests:
  // keying on `ordinal === undefined` looks equivalent, because the echo never
  // stamps one and the cursor outruns the durable twin — so EVERY user row this
  // client rendered live stays ordinal-less for good. A restored thread meeting
  // any REPLACE narrower than itself then had months of settled questions torn
  // out of place and welded below the newest answer.
  it("DROPS settled sends outside a narrow page — the outbox stopped owing them", () => {
    const prev: Row[] = [
      { id: "pm-old", role: "user", content: "asked in March" },
      { id: "m621", role: "assistant", ordinal: 621, content: "answered" },
      { id: "pm-mid", role: "user", content: "asked in April" },
      { id: "m987", role: "assistant", ordinal: 987, content: "answered" },
    ];
    const page: Row[] = [{ id: "m987", role: "assistant", ordinal: 987, content: "answered" }];
    expect(applySyncReplace(prev, page, owed()).map((r) => r.id)).toEqual(["m987"]);
  });

  it("keeps a still-owed send even beside settled ones a narrow page drops", () => {
    const prev: Row[] = [
      { id: "pm-old", role: "user", content: "asked in March" },
      { id: "m987", role: "assistant", ordinal: 987, content: "answered" },
      { id: "pm-live", role: "user", content: "just now", sendState: "sending" },
    ];
    const page: Row[] = [{ id: "m987", role: "assistant", ordinal: 987, content: "answered" }];
    expect(applySyncReplace(prev, page, owed("pm-live")).map((r) => r.id)).toEqual(["m987", "pm-live"]);
  });

  // The page is a SNAPSHOT: its newest ordinal is when the server took it. A
  // reply persisted after that arrives over the socket carrying its own ordinal,
  // and that frame advances the cursor — so dropping the row is permanent, since
  // a difference selects strictly `>` the cursor the row itself set. A cold open
  // is the one path that runs a baseline, which is where this read as "the
  // newest message never arrives".
  it("keeps a live reply the page predates", () => {
    const prev: Row[] = [
      { id: "m10", role: "assistant", ordinal: 10, content: "older answer" },
      { id: "m12", role: "assistant", ordinal: 12, content: "the newest" },
    ];
    const page: Row[] = [{ id: "m10", role: "assistant", ordinal: 10, content: "older answer" }];
    expect(applySyncReplace(prev, page, owed()).map((r) => r.id)).toEqual(["m10", "m12"]);
  });

  it("keeps it behind the page's own rows, and never twice", () => {
    const prev: Row[] = [{ id: "m12", role: "assistant", ordinal: 12, content: "the newest" }];
    const page: Row[] = [
      { id: "m10", role: "assistant", ordinal: 10, content: "a" },
      { id: "m12", role: "assistant", ordinal: 12, content: "the newest" },
    ];
    expect(applySyncReplace(prev, page, owed()).map((r) => r.id)).toEqual(["m10", "m12"]);
  });

  // NOTE: the frame path now enrols EVERY ordinal-less user echo it appends —
  // a second paired device's included, knowingly (kept-and-possibly-re-filed
  // beats deleted) — so a rendered echo row reaches this not-owed state only
  // through RETIREMENT (a page carried its pmid, or native's `sendConfirmed`),
  // never on arrival. This pins the pure contract for that retired state.
  it("DROPS an ordinal-less user row the kept set no longer holds", () => {
    const prev: Row[] = [{ id: "pm-laptop", role: "user", content: "sent from my laptop" }];
    expect(applySyncReplace(prev, [{ id: "m10", role: "assistant", content: "a" }], owed())).toEqual([
      { id: "m10", role: "assistant", content: "a" },
    ]);
  });

  it("carries the single newest open block, dropping an older stale fork", () => {
    const prev: Row[] = [
      work({ id: "u-old", steps: [tool({ callId: "c1" })], active: true }),
      work({ id: "u-new", steps: [tool({ callId: "c2" })], active: true }),
    ];
    const out = applySyncReplace(prev, [{ id: "m10", role: "assistant", content: "a" }], owed());
    expect(out.map((r) => r.id)).toEqual(["m10", "u-new"]);
    // A page that ends on an ANSWER says the turn is over. With no block on the
    // page to fold into we keep what we hold rather than delete it — but it
    // stops being a live card, or it spins "Working" behind a settled reply for
    // as long as the session lasts.
    expect(out[1]).toMatchObject({ active: false });
  });

  // The `[work][reply][work]` duplicate, REPLACE edition: the offscreen buffer
  // overflowed and ate the turn's `turn_state{inactive}`, so the block is still
  // open here while the rebase page — which ends on that turn's answer — already
  // carries the whole turn. Appending is what put a second card under the reply.
  it("folds a stranded open block into the page's own block for that turn", () => {
    // We streamed c1 before the buffer overflowed; the page holds the whole turn
    // (persistence is the superset), and that shared step is the proof they are
    // one turn.
    const prev: Row[] = [work({ id: "u-live", steps: [tool({ callId: "c1" })], active: true })];
    const page: Row[] = [
      { id: "pm-1", role: "user", content: "go", ordinal: 9 },
      work({
        id: "w10",
        steps: [tool({ callId: "c1" }), tool({ callId: "c2" })],
        turnComplete: true,
        elapsedMs: 4_000,
      }),
      { id: "m11", role: "assistant", ordinal: 11, content: "done" },
    ];
    const out = applySyncReplace(prev, page, owed());
    expect(out.map((r) => r.id)).toEqual(["pm-1", "w10", "m11"]);
    expect((out[1] as WorkRow).steps.map((s) => (s.kind === "tool" ? s.callId : s.kind))).toEqual(["c1", "c2"]);
    expect(out[1]).toMatchObject({ active: false, elapsedMs: 4_000 });
  });

  // Adjacency is not evidence. A delivery turn carries no user row, so both kept
  // sets are empty while its block is genuinely the NEXT turn's — and welding it
  // into the card above would bury one turn inside another, permanently.
  it("refuses to fold an open block that shares no step with the page's block", () => {
    const prev: Row[] = [work({ id: "u-live", steps: [tool({ callId: "c9" })], active: true })];
    const page: Row[] = [
      work({ id: "w10", steps: [tool({ callId: "c1" })], turnComplete: true }),
      { id: "m11", role: "assistant", ordinal: 11, content: "done" },
    ];
    const out = applySyncReplace(prev, page, owed());
    expect(out.map((r) => r.id)).toEqual(["w10", "m11", "u-live"]);
    expect((out[0] as WorkRow).steps).toHaveLength(1);
    expect(out[2]).toMatchObject({ active: false });
  });

  // …but an owed send below the page proves a LATER turn, and that turn's block
  // is genuinely live — it must keep its own card at the tail.
  it("keeps the open block when an owed send proves it belongs to a later turn", () => {
    const prev: Row[] = [
      { id: "pm-2", role: "user", content: "next", sendState: "sending" },
      work({ id: "u-live", steps: [tool({ callId: "c2" })], active: true }),
    ];
    const page: Row[] = [
      work({ id: "w10", steps: [tool({ callId: "c1" })], turnComplete: true }),
      { id: "m11", role: "assistant", ordinal: 11, content: "done" },
    ];
    const out = applySyncReplace(prev, page, owed("pm-2"));
    expect(out.map((r) => r.id)).toEqual(["w10", "m11", "pm-2", "u-live"]);
    expect(out[3]).toMatchObject({ active: true });
  });

  it("fuses the page's cut-off trailing block with the live one — one turn, one card", () => {
    const prev: Row[] = [work({ id: "u-live", steps: [tool({ callId: "c2" })], active: true })];
    const page: Row[] = [work({ id: "w20", steps: [tool({ callId: "c1" })], turnComplete: false, startedAt: 20_000 })];
    const out = applySyncReplace(prev, page, owed());
    expect(out).toHaveLength(1);
    expect(out[0]).toMatchObject({ id: "w20", active: true, startedAt: 20_000 });
    expect((out[0] as WorkRow).steps.map((s) => (s.kind === "tool" ? s.callId : s.kind))).toEqual(["c1", "c2"]);
  });

  // The whole reported bug, end to end: send → echo → the REPLACE that raced
  // persistence → work → answer → the outbox's mount-edge replay.
  it("never lands the first send below the answer", () => {
    const markSent = (rows: Row[], id: string, ordinal: number | null): Row[] =>
      rows.map((r) => (r.role === "user" && r.id === id ? { ...r, sendState: undefined, ordinal: ordinal ?? r.ordinal } : r));

    const owing = new Set<string>(["pm-1"]); // `handleUserSent` minted it
    let rows: Row[] = [{ id: "pm-1", role: "user", content: "hi", sendState: "sending" }];
    rows = markSent(rows, "pm-1", null); // the echo: no ordinal, `channel/adapter.rs`
    rows = applySyncReplace(rows, [], owing); // the baseline whose page predates the write
    expect(rows.map((r) => r.id)).toEqual(["pm-1"]); // (A) it survives

    rows = [...rows, work({ id: "u-live", steps: [tool()], active: true })]; // (B)
    rows = [...rows, { id: "m12", role: "assistant", ordinal: 12, content: "there" }];

    // (C) the mount edge replays every send the outbox still owns; the guard
    // that makes it idempotent is the row being there to find.
    expect(holdsUserSend(rows, "pm-1")).toBe(true);
    expect(rows.map((r) => r.id)).toEqual(["pm-1", "u-live", "m12"]);
  });

  // A session's rows are never deleted, so an empty page against a thread that
  // holds rows is always a read the gateway served before this session's first
  // row persisted. Applying it re-filed the survivors — kept sets return
  // BEHIND the page — and deleted outright any row that had lost (or never
  // gained) its kept-set membership, which is exactly the row an eaten
  // `userSent` leaves behind: the echo-appended bubble.
  it("treats an empty page against a non-empty thread as stale — nothing moves, owed or not", () => {
    const prev: Row[] = [
      { id: "pm-1", role: "user", content: "hi" }, // echo-appended: no membership
      { id: "m12", role: "assistant", ordinal: 12, content: "there" },
    ];
    expect(applySyncReplace(prev, [], owed())).toBe(prev);
  });

  // The other interleaving of the end-to-end scar above: the answer arrives
  // BEFORE the raced baseline's empty response does. The kept sets used to
  // return behind the page — `[reply, first send]`, with the closed work card
  // (ordinal-less, inactive: in neither set) deleted — and the order persisted
  // into the mirror, past every later sync (the reply's ordinal outran the
  // cursor) and every re-entry.
  it("never re-files the first send below a reply the empty page predates", () => {
    const owing = new Set<string>(["pm-1"]);
    let rows: Row[] = [{ id: "pm-1", role: "user", content: "hi", sendState: "sending" }];
    rows = rows.map((r) => (r.id === "pm-1" ? { ...r, sendState: undefined } : r)); // the echo
    rows = [...rows, work({ id: "u-live", steps: [tool()], active: false })]; // turn ended
    rows = [...rows, { id: "m12", role: "assistant", ordinal: 12, content: "there" }];

    rows = applySyncReplace(rows, [], owing); // the raced baseline, landing LAST
    expect(rows.map((r) => r.id)).toEqual(["pm-1", "u-live", "m12"]);
  });

  // A confirmed send is stamped AND (briefly, under a call-site ordering slip)
  // still owed — the two kept sets must stay exclusive or the row renders
  // twice under one React key.
  it("emits a row exactly once when it is both owed and above the ceiling", () => {
    const prev: Row[] = [{ id: "pm-1", role: "user", ordinal: 5, content: "hi" }];
    const page: Row[] = [{ id: "m4", role: "assistant", ordinal: 4, content: "a" }];
    expect(applySyncReplace(prev, page, owed("pm-1")).map((r) => r.id)).toEqual(["m4", "pm-1"]);
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

describe("compactionDividerIds — the pre-compaction seam", () => {
  const row = (id: string): Row => ({ id }) as Row;
  // Machinery ordinals (3..6) are hidden server-side, so the thread jumps 2 → 7.
  const thread = [row("m0"), row("m1"), row("m2"), row("m7"), row("m8")];

  it("marks the first row at/after the watermark, once", () => {
    const ids = compactionDividerIds(thread, [{ ordinal: 3, at: "2026-07-22T10:00:00Z" }]);
    expect([...ids.entries()]).toEqual([["m7", "2026-07-22T10:00:00Z"]]);
  });

  it("draws nothing when the boundary is above every loaded row", () => {
    const ids = compactionDividerIds([row("m7"), row("m8")], [{ ordinal: 3, at: "t" }]);
    expect(ids.size).toBe(0);
  });

  it("is empty when the session was never compacted", () => {
    expect(compactionDividerIds(thread, []).size).toBe(0);
  });

  it("handles two compactions at their own seams", () => {
    const twice = [row("m0"), row("m4"), row("m9")];
    const ids = compactionDividerIds(twice, [
      { ordinal: 2, at: "t1" },
      { ordinal: 6, at: "t2" },
    ]);
    expect([...ids.entries()]).toEqual([
      ["m4", "t1"],
      ["m9", "t2"],
    ]);
  });

  it("ignores rows with no ordinal (interleaved notices / live blocks)", () => {
    const withNotice = [row("m2"), row("n5"), row("m7")];
    const ids = compactionDividerIds(withNotice, [{ ordinal: 3, at: "t" }]);
    expect([...ids.keys()]).toEqual(["m7"]);
  });
});

describe("outlineEntries — the message index's model", () => {
  const said = (id: string, content: string, over: Partial<ChatMsg> = {}): ChatMsg => ({
    id,
    role: "user",
    content,
    ...over,
  });
  const replied = (id: string, content: string): ChatMsg => ({ id, role: "assistant", content });
  const notice = (id: string, content: string): ChatMsg => ({ id, role: "notice", content });

  it("glosses a prompt with its answer, scanning PAST the turn's work and notice rows", () => {
    const rows: Row[] = [
      said("p1", "ship it"),
      work({ id: "w1", steps: [tool()] }),
      notice("n1", "skill degraded"),
      replied("m2", "**Shipped.**"),
    ];
    expect(outlineEntries(rows)).toEqual([
      { id: "p1", text: "ship it", gloss: "Shipped.", at: "", dayKey: "", attachments: 0, state: undefined },
    ]);
  });

  it("STOPS at the next user row — a prompt must never borrow the following turn's answer", () => {
    const rows: Row[] = [said("p1", "first"), said("p2", "second"), replied("m3", "answering the second")];
    expect(outlineEntries(rows).map((e) => e.gloss)).toEqual(["", "answering the second"]);
  });

  it("leaves the gloss empty for a still-running turn (nothing has been said back yet)", () => {
    const rows: Row[] = [said("p1", "go"), work({ id: "w1", active: true, steps: [tool()] })];
    expect(outlineEntries(rows)[0].gloss).toBe("");
  });

  it("leaves the gloss empty for a STOPPED turn — the mark is not an answer", () => {
    const rows: Row[] = [
      said("p1", "go"),
      work({ id: "w1", steps: [tool()] }),
      { id: "n1", role: "notice", content: "", stopped: true },
    ];
    expect(outlineEntries(rows)[0].gloss).toBe("");
  });

  it("lists nothing for a `/stop` — the command never reaches `messages`, so the sheet cannot offer it", () => {
    // Exactly what the thread holds after the echo drop: two real sends, no
    // `/stop` row between them.
    const rows: Row[] = [said("p1", "run it"), work({ id: "w1" }), said("p2", "again")];
    expect(outlineEntries(rows).map((e) => e.id)).toEqual(["p1", "p2"]);
  });

  it("carries an attachment-only send by its COUNT — its text is empty and the gloss identifies it", () => {
    const rows: Row[] = [
      said("p1", "", {
        attachments: [
          { kind: "image", blob_id: "sha256:a.tok", mime_type: "image/png", size: 1 },
          { kind: "file", blob_id: "sha256:b.tok", mime_type: "text/plain", size: 2 },
        ],
      }),
      replied("m2", "Two files, both fine."),
    ];
    expect(outlineEntries(rows)[0]).toMatchObject({
      text: "",
      attachments: 2,
      gloss: "Two files, both fine.",
    });
  });

  it("passes an unconfirmed send's state through — the sheet greys it the way the bubble does", () => {
    const rows: Row[] = [said("p1", "a", { sendState: "sending" }), said("p2", "b", { sendState: "failed" })];
    expect(outlineEntries(rows).map((e) => e.state)).toEqual(["sending", "failed"]);
  });

  it("collapses whitespace and caps text AND gloss at the transport limit", () => {
    const rows: Row[] = [said("p1", `x${"y".repeat(400)}`), replied("m2", "z".repeat(400))];
    const [entry] = outlineEntries(rows);
    expect(entry.text).toHaveLength(160);
    expect(entry.gloss).toHaveLength(160);
    expect(outlineEntries([said("p2", "  two\n\n  lines  ")])[0].text).toBe("two lines");
  });

  // A `.slice` cap cuts UTF-16 units, so a non-BMP character straddling the cap
  // leaves a lone surrogate — and native re-serializes this payload with
  // JSONSerialization, which THROWS on one, blanking the whole index for the
  // conversation on every repost. Cutting by code point makes that impossible.
  it("never cuts a surrogate pair in half, however the cap falls", () => {
    for (let lead = 155; lead <= 165; lead++) {
      const text = `${"a".repeat(lead)}🎉 tail`;
      const [entry] = outlineEntries([said("p1", text), replied("m2", text)]);
      for (const field of [entry.text, entry.gloss]) {
        expect(Array.from(field).length).toBeLessThanOrEqual(160);
        // Throws `URIError` on a lone surrogate — the same "can this become
        // UTF-8" question native's JSONSerialization asks.
        expect(() => encodeURIComponent(field)).not.toThrow();
      }
    }
  });

  it("leaves `at` and `dayKey` empty when the row carries no createdAt (a pre-timestamp mirror)", () => {
    const [entry] = outlineEntries([said("p1", "hi")]);
    expect(entry.at).toBe("");
    expect(entry.dayKey).toBe("");
  });

  it("keys the day in DEVICE-LOCAL time — toISOString would file an evening message under tomorrow", () => {
    vi.stubEnv("TZ", "Asia/Tokyo");
    try {
      const iso = "2026-07-22T23:30:00Z"; // 2026-07-23 08:30 in Tokyo
      const [entry] = outlineEntries([said("p1", "hi", { createdAt: iso })]);
      expect(entry.dayKey).toBe("2026-07-23");
      expect(entry.at).not.toBe("");
    } finally {
      vi.unstubAllEnvs();
    }
  });
});

describe("hasSubagentSpawn — what lights the header's Subagents entry", () => {
  const spawn = tool({ callId: "c9", tool: "spawn_subagent", label: "audit the sync loop" });

  it("is false for a thread whose work is ordinary tool calls", () => {
    const rows: Row[] = [
      { id: "p1", role: "user", content: "ship it" },
      work({ id: "w1", steps: [{ kind: "reasoning", text: "thinking" }, tool({ tool: "bash" })] }),
    ];
    expect(hasSubagentSpawn(rows)).toBe(false);
  });

  it("is true once any block ran the spawn tool", () => {
    const rows: Row[] = [
      work({ id: "w1", steps: [tool({ tool: "read" })] }),
      work({ id: "w2", steps: [spawn] }),
    ];
    expect(hasSubagentSpawn(rows)).toBe(true);
  });

  // The gateway's label is pulled from the call INPUT — for a spawn it names the
  // errand — so the display string can never stand in for the tool name.
  it("reads the tool name, NOT the label the step renders", () => {
    expect(hasSubagentSpawn([work({ id: "w1", steps: [spawn] })])).toBe(true);
    expect(
      hasSubagentSpawn([
        work({ id: "w1", steps: [tool({ tool: "bash", label: "spawn_subagent" })] }),
      ]),
    ).toBe(false);
  });

  // A mirror written before the step carried its tool name — the native side's
  // own list pull is what covers that conversation until a sync re-delivers it.
  it("is false for a step that carries no tool name at all", () => {
    expect(hasSubagentSpawn([work({ id: "w1", steps: [tool({ tool: undefined })] })])).toBe(false);
  });
});

describe("flattenGloss — markdown onto one line", () => {
  it("drops a fenced block whole — a gloss must not read as source", () => {
    expect(flattenGloss("before\n```js\nconst x = 1;\n```\nafter")).toBe("before after");
  });

  it("drops an UNCLOSED fence to end of input (the mid-stream state)", () => {
    expect(flattenGloss("before\n~~~\nstill streaming")).toBe("before");
  });

  it("strips heading, bullet and emphasis marks, keeping `_` (it is snake_case far more often)", () => {
    expect(flattenGloss("# Title\n- **bold** and `code`\n- a_b")).toBe("Title bold and code a_b");
  });

  // `~` only marks up when doubled, and this codebase's replies are full of
  // `~/paths` and `~50ms` approximations that a blanket strip would corrupt.
  it("strips `~~strikethrough~~` but leaves a lone tilde alone", () => {
    expect(flattenGloss("wrote it to ~/bin/deploy.sh in ~50ms")).toBe(
      "wrote it to ~/bin/deploy.sh in ~50ms",
    );
    expect(flattenGloss("~~dropped~~ kept")).toBe("dropped kept");
  });

  it("keeps a link's TEXT and drops its target", () => {
    expect(flattenGloss("see [the docs](https://x.dev/guide) now")).toBe("see the docs now");
    expect(flattenGloss("see [the docs][ref] now")).toBe("see the docs now");
    expect(flattenGloss("![a screenshot](x.png)")).toBe("a screenshot");
  });

  it("stays linear on a 40KB paste — a backtracking pattern here would hang the transcript", () => {
    const huge = `${"[unclosed *emphasis* ".repeat(2_000)}\n\`\`\`\n${"code ".repeat(2_000)}`;
    expect(huge.length).toBeGreaterThan(40_000);
    const started = Date.now();
    expect(flattenGloss(huge).length).toBeGreaterThan(0);
    expect(Date.now() - started).toBeLessThan(1_000);
  });
});

// The bundle's judgement about the reply on screen is three-valued, and the
// third value is the one that matters: a bundle carrying NO answer text says
// nothing about the reply, so clearing there deletes a paragraph mid-read.
// Mirrors `bundleAnswer` in app/web/src/pages/ChatPage.tsx.
describe("bundleAnswer", () => {
  it("recovers the trailing prose as the answer in flight", () => {
    expect(bundleAnswer([tool({ callId: "c1" }), { kind: "prose", text: "答案" }])).toEqual({
      kind: "recovered",
      text: "答案",
    });
  });

  it("calls a reply superseded when the bundle holds the text but moved past it", () => {
    expect(bundleAnswer([{ kind: "prose", text: "叙述" }, tool({ callId: "c1" })])).toEqual({
      kind: "superseded",
    });
  });

  it("says nothing when the bundle carries no answer text at all", () => {
    expect(bundleAnswer([])).toEqual({ kind: "unknown" });
    expect(bundleAnswer([{ kind: "reasoning", text: "r" }, tool({ callId: "c1" })])).toEqual({
      kind: "unknown",
    });
  });
});

describe("splitForFirstPaint — the mirror's older half is deferred past the first commit", () => {
  const msg = (ordinal: number): Row => ({
    id: `m${ordinal}`,
    role: "assistant",
    content: `row ${ordinal}`,
  });
  const many = (count: number, from = 0): Row[] =>
    Array.from({ length: count }, (_, i) => msg(from + i));

  it("leaves a thread at or under the threshold whole — a short chat's open is untouched", () => {
    const rows = many(FIRST_PAINT_ROWS);
    expect(splitForFirstPaint(rows)).toEqual({ head: [], tail: rows });
  });

  it("paints the NEWEST rows and defers the rest — the reader sees the bottom, not the top", () => {
    const rows = many(FIRST_PAINT_ROWS + 12);
    const { head, tail } = splitForFirstPaint(rows);
    expect(tail).toHaveLength(FIRST_PAINT_ROWS);
    expect(tail[tail.length - 1]).toBe(rows[rows.length - 1]);
    expect(head).toHaveLength(12);
  });

  it("strands nothing — head ++ tail is the input, in order", () => {
    const rows = many(FIRST_PAINT_ROWS + 7);
    const { head, tail } = splitForFirstPaint(rows);
    expect([...head, ...tail]).toEqual(rows);
  });

  it("oldestRowOrdinal reads the paging floor off the rendered half", () => {
    const { tail } = splitForFirstPaint(many(FIRST_PAINT_ROWS + 5));
    expect(oldestRowOrdinal(tail)).toBe(5);
  });

  it("oldestRowOrdinal is null when nothing in the thread carries an ordinal", () => {
    expect(oldestRowOrdinal([{ id: "n1", role: "notice", content: "stopped" }])).toBeNull();
  });

  // The invariant the whole change rests on. `syncSince` is the gate that decides
  // between a difference and a baseline REPLACE, and it answers by scanning the
  // RENDERED rows for a durable ordinal above the cursor. Deferring rows can only
  // remove candidates BELOW the ones it looks for, so its answer has to be the
  // same either way — otherwise a long chat's open would take a different sync
  // path from a short one, which is the class of bug that scrambled a thread once
  // already (see the describe above).
  describe("the sync gate cannot tell the thread was split", () => {
    const rows = many(FIRST_PAINT_ROWS + 20);
    const { tail } = splitForFirstPaint(rows);

    it("agrees on a thread the cursor covers (a difference)", () => {
      const cursor = FIRST_PAINT_ROWS + 19;
      expect(syncSince(cursor, tail)).toBe(syncSince(cursor, rows));
      expect(syncSince(cursor, tail)).toBe(cursor);
    });

    it("agrees on a thread the cursor does NOT cover (a baseline REPLACE)", () => {
      expect(syncSince(5, tail)).toBe(syncSince(5, rows));
      expect(syncSince(5, tail)).toBeNull();
    });
  });

  // `drainDeferredHead` re-joins through `prependOlder`, which folds work blocks
  // at the seam — so the split must not manufacture a fold that the unsplit
  // mirror never had. It can't: `sanitizeRestoredRows` has already folded this
  // array, so any adjacent work pair still standing is one it deliberately left
  // apart, and `sameContinuingTurn` refuses exactly that shape. Pinned on the
  // real functions at the real boundary, both directions.
  describe("re-joining the split is a no-op on the work-block fold", () => {
    // `count` rows with two adjacent work blocks landing either side of the cut.
    const straddling = (headTurnComplete: boolean): Row[] => {
      const tailLen = FIRST_PAINT_ROWS;
      const rows: Row[] = [
        msg(0),
        work({ id: "w1", steps: [tool({ callId: "c1" })], turnComplete: headTurnComplete }),
        work({ id: "w2", steps: [tool({ callId: "c2" })], turnComplete: true }),
        ...many(tailLen - 1, 10),
      ];
      // The cut must fall exactly between w1 and w2.
      const { head, tail } = splitForFirstPaint(rows);
      expect(head[head.length - 1].id).toBe("w1");
      expect(tail[0].id).toBe("w2");
      return rows;
    };

    it("keeps two COMPLETE turns apart — the cut cannot weld them into one card", () => {
      const sanitized = sanitizeRestoredRows(straddling(true));
      const { head, tail } = splitForFirstPaint(sanitized);
      expect(foldAdjacentWork([...head, ...tail], [])).toEqual(sanitized);
      expect(foldAdjacentWork([...head, ...tail], []).filter((r) => r.role === "work")).toHaveLength(2);
    });

    it("and a CUT-OFF head was already folded by the restore, so the cut can't land between them", () => {
      const sanitized = sanitizeRestoredRows(straddling(false));
      expect(sanitized.filter((r) => r.role === "work")).toHaveLength(1);
      const { head, tail } = splitForFirstPaint(sanitized);
      expect(foldAdjacentWork([...head, ...tail], [])).toEqual(sanitized);
    });
  });
});
