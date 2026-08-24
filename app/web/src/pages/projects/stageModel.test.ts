import { describe, expect, it } from 'vitest';

import type { Issue } from './boardModel';
import { groupByStage, stageProgress } from './stageModel';

let seq = 0;
function child(stage: number, overrides: Partial<Issue> = {}): Issue {
  seq += 1;
  return {
    number: seq,
    project_id: '01JP',
    title: `step ${seq}`,
    description: '',
    status: 'backlog',
    priority: 'none',
    position: 0,
    stage,
    created_at_ms: 0,
    updated_at_ms: 0,
    unread: 0,
    last_run_failed: false,
    opened_by_agent: false,
    pinned: false,
    ...overrides,
  };
}

describe('groupByStage', () => {
  it('opens only the first stage with work left', () => {
    const grouped = groupByStage([
      child(0, { status: 'done' }),
      child(1),
      child(2),
      child(1, { status: 'in_progress' }),
    ]);
    expect(grouped.map((s) => [s.stage, s.state])).toEqual([
      [0, 'done'],
      [1, 'open'],
      [2, 'waiting'],
    ]);
  });

  it('treats a cancelled step as finished', () => {
    const grouped = groupByStage([
      child(0, { status: 'done' }),
      child(0, { cancelled_at_ms: 1 }),
      child(1),
    ]);
    expect(grouped[0].state).toBe('done');
    expect(grouped[1].state).toBe('open');
  });

  it('sorts stages numerically, not as strings', () => {
    const grouped = groupByStage([child(10), child(2), child(1)]);
    expect(grouped.map((s) => s.stage)).toEqual([1, 2, 10]);
  });

  it('reports every stage done when nothing is pending', () => {
    const grouped = groupByStage([child(0, { status: 'done' }), child(1, { status: 'done' })]);
    expect(grouped.every((s) => s.state === 'done')).toBe(true);
  });

  it('has nothing to group for a card with no steps', () => {
    expect(groupByStage([])).toEqual([]);
  });
});

describe('stageProgress', () => {
  it('counts only work still meant to happen', () => {
    expect(
      stageProgress([
        child(0, { status: 'done' }),
        child(0, { status: 'done' }),
        child(1, { cancelled_at_ms: 1 }),
      ]),
    ).toEqual({ done: 2, total: 2 });
    expect(stageProgress([])).toEqual({ done: 0, total: 0 });
  });
});
