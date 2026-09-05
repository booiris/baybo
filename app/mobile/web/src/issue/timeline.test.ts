import { describe, expect, it } from "vitest";

import { agentQuestion, fold, foldHead, isAlwaysShown, pendingApprovals, type Fold } from "./timeline";
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

  it("folds an unknown kind as machinery rather than dropping it", () => {
    const folded = fold([event("e1", "swimlane_changed", { lane: "fast" })]);
    expect(folded).toHaveLength(1);
    expect(folded[0]?.kind).toBe("system");
  });

  it("an empty timeline folds to nothing", () => {
    expect(fold([])).toEqual([]);
  });

  it("splits a run of machinery at the unread boundary", () => {
    const events = [
      event("e1", "moved", { from: "todo", to: "in_progress" }),
      event("e2", "run_started", { attempt: 1 }),
      event("e3", "run_settled", { attempt: 1, status: "done" }),
    ];
    expect(fold(events).map((f) => f.kind)).toEqual(["system"]);

    const split = fold(events, "e2");
    expect(split).toHaveLength(2);
    expect(split[0]?.kind === "system" && split[0].events.map((e) => e.id)).toEqual(["e1"]);
    expect(split[1]?.kind === "system" && split[1].events.map((e) => e.id)).toEqual(["e2", "e3"]);
  });

  it("names the row a fold is drawn at, which is what the rule anchors to", () => {
    const machinery = fold([event("e1", "moved"), event("e2", "run_started")]);
    expect(foldHead(machinery[0] as Fold)?.id).toBe("e1");
    const comment = fold([event("e9", "comment", { text: "hi" }, agent("dev-1"))]);
    expect(foldHead(comment[0] as Fold)?.id).toBe("e9");
  });

  it("an unknown boundary changes nothing", () => {
    const events = [event("e1", "moved"), event("e2", "run_started")];
    expect(fold(events, "e404")).toEqual(fold(events));
  });
});

describe("pending approvals", () => {
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

  it("the newest block decides", () => {
    const timeline = [
      event("e1", "blocked", { reason: "first" }, agent("lead")),
      event("e2", "blocked", { reason: "second" }, { kind: "user" }),
    ];
    expect(agentQuestion("second", timeline)).toBeNull();
  });
});
