import { describe, expect, it } from "vitest";

import { agentQuestion, fold, isAlwaysShown, pendingApprovals } from "./timeline";
import type { Actor, IssueEvent } from "./types";

function event(id: string, kind: string, extra: Record<string, unknown> = {}, actor: Actor = { kind: "system" }): IssueEvent {
  return { id, number: 1, actor, body: { kind, ...extra }, created_at_ms: Number(id.slice(1)) };
}

const agent = (handle: string): Actor => ({ kind: "agent", id: `a-${handle}`, handle });

describe("activity fold", () => {
  it("collapses consecutive machinery and never a comment", () => {
    const folded = fold([
      event("e1", "moved", { from: "todo", to: "in_progress" }),
      event("e2", "run_started", { attempt: 1 }),
      event("e3", "comment", { text: "looking" }, agent("dev-1")),
      event("e4", "run_settled", { attempt: 1, status: "done" }),
    ]);
    expect(folded.map((f) => f.kind)).toEqual(["system", "entry", "system"]);
    expect(folded[0]?.kind === "system" && folded[0].events.map((e) => e.id)).toEqual(["e1", "e2"]);
  });

  it("keeps approvals and blocks out of the fold — they are why the card was opened", () => {
    for (const kind of ["comment", "approval_requested", "approval_resolved", "blocked", "unblocked"]) {
      expect(isAlwaysShown(event("e1", kind))).toBe(true);
    }
    for (const kind of ["moved", "run_started", "worktree_reclaimed"]) {
      expect(isAlwaysShown(event("e1", kind))).toBe(false);
    }
  });

  /// A kind this build has never heard of must still fold and still render —
  /// the gateway adds kinds on its own schedule, and a card whose Activity
  /// threw would take its comments down with it.
  it("folds an unknown kind as machinery rather than dropping it", () => {
    const folded = fold([event("e1", "swimlane_changed", { lane: "fast" })]);
    expect(folded).toHaveLength(1);
    expect(folded[0]?.kind).toBe("system");
  });

  it("an empty timeline folds to nothing", () => {
    expect(fold([])).toEqual([]);
  });
});

describe("pending approvals", () => {
  /// Retired by `call_id`, not by "the newest resolution wins" — one card can
  /// hold several across a run and a resolution retires exactly one.
  it("a resolution retires its own prompt and leaves the rest", () => {
    const open = pendingApprovals([
      event("e1", "approval_requested", { call_id: "c1", tool: "exec_command" }),
      event("e2", "approval_requested", { call_id: "c2", tool: "exec_command" }),
      event("e3", "approval_resolved", { call_id: "c1", decision: "approve" }),
    ]);
    expect(open.map((e) => e.body.call_id)).toEqual(["c2"]);
  });

  it("a re-request after a resolution reopens the prompt", () => {
    const open = pendingApprovals([
      event("e1", "approval_requested", { call_id: "c1" }),
      event("e2", "approval_resolved", { call_id: "c1" }),
      event("e3", "approval_requested", { call_id: "c1" }),
    ]);
    expect(open.map((e) => e.id)).toEqual(["e3"]);
  });

  it("an entry with no call id is not a prompt", () => {
    expect(pendingApprovals([event("e1", "approval_requested", {})])).toEqual([]);
  });
});

describe("agent question", () => {
  /// An OPERATOR's block is that operator saying stop, and nothing should
  /// invite them to answer themselves.
  it("only an agent-authored block is a question", () => {
    const byAgent = [event("e1", "blocked", { reason: "which token?" }, agent("lead"))];
    expect(agentQuestion("which token?", byAgent)).toEqual({
      askedBy: "lead",
      question: "which token?",
    });
    const byOperator = [event("e1", "blocked", { reason: "stop" }, { kind: "user" })];
    expect(agentQuestion("stop", byOperator)).toBeNull();
  });

  it("an unblocked card asks nothing, whatever its history says", () => {
    expect(agentQuestion(undefined, [event("e1", "blocked", {}, agent("lead"))])).toBeNull();
    expect(agentQuestion("", [event("e1", "blocked", {}, agent("lead"))])).toBeNull();
  });

  /// The NEWEST block is the one in force: an earlier one may have been lifted
  /// and re-applied by somebody else entirely.
  it("the newest block decides", () => {
    const timeline = [
      event("e1", "blocked", { reason: "first" }, agent("lead")),
      event("e2", "blocked", { reason: "second" }, { kind: "user" }),
    ];
    expect(agentQuestion("second", timeline)).toBeNull();
  });
});
