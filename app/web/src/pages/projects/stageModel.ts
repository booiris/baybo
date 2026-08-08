import type { Issue } from './boardModel';

export type Stage = {
  stage: number;
  issues: Issue[];
  state: 'done' | 'open' | 'waiting';
};

function isPending(issue: Issue): boolean {
  return issue.cancelled_at_ms == null && issue.status !== 'done';
}

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

export function stageProgress(children: Issue[]): { done: number; total: number } {
  const live = children.filter((child) => child.cancelled_at_ms == null);
  return {
    done: live.filter((child) => child.status === 'done').length,
    total: live.length,
  };
}
