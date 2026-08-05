import type { Issue } from './boardModel';

/**
 * One barrier under a parent card.
 *
 * `state` is what a reader needs to know at a glance and cannot get from
 * the rows alone: whether this stage is finished, whether it is the one
 * being worked, or whether it is still waiting behind an earlier one.
 */
export type Stage = {
  stage: number;
  issues: Issue[];
  state: 'done' | 'open' | 'waiting';
};

/** A step still to happen. Cancelled counts as finished — a stage waiting
 *  on work somebody called off would never open, and cancelling the step
 *  is precisely how an operator unblocks one. Mirrors `is_pending` in
 *  `crates/project/src/stages.rs`. */
function isPending(issue: Issue): boolean {
  return issue.cancelled_at_ms == null && issue.status !== 'done';
}

/**
 * A parent's children, grouped into the barriers the server enforces.
 *
 * The **first** stage with pending work is `open`; every later one is
 * `waiting`, because that is what the barrier actually does — stage N+1
 * starts when N empties. Marking them all "open" would tell the operator
 * work is available that the server will not start.
 */
export function groupByStage(children: Issue[]): Stage[] {
  const byStage = new Map<number, Issue[]>();
  for (const child of children) {
    const bucket = byStage.get(child.stage);
    if (bucket === undefined) byStage.set(child.stage, [child]);
    else bucket.push(child);
  }
  const stages = [...byStage.entries()].sort(([a], [b]) => a - b);
  let seenOpen = false;
  return stages.map(([stage, issues]) => {
    const pending = issues.some(isPending);
    if (!pending) return { stage, issues, state: 'done' as const };
    if (seenOpen) return { stage, issues, state: 'waiting' as const };
    seenOpen = true;
    return { stage, issues, state: 'open' as const };
  });
}

/**
 * `done / total` for a parent, counting only work still meant to happen.
 *
 * Cancelled children leave both counts, so a card whose last two steps were
 * called off reads `3/3` rather than `3/5` — the question a reader is
 * asking is how much of the remaining work is done. Mirrors `progress` in
 * `crates/project/src/stages.rs`.
 */
export function stageProgress(children: Issue[]): { done: number; total: number } {
  const live = children.filter((child) => child.cancelled_at_ms == null);
  return {
    done: live.filter((child) => child.status === 'done').length,
    total: live.length,
  };
}
