import { describe, expect, it } from 'vitest';

import {
  attentionFor,
  attentionSummary,
  boardsNeedingAttention,
  type ProjectAttention,
} from './useAttention';

function board(overrides: Partial<ProjectAttention> = {}): ProjectAttention {
  return {
    project_id: '01JA',
    name: 'Alpha',
    approvals: 0,
    held: 0,
    failed: 0,
    unread: 0,
    ...overrides,
  };
}

describe('boardsNeedingAttention', () => {
  it('counts boards, not the things on them', () => {
    const boards = [
      board({ approvals: 3, held: 2 }),
      board({ project_id: '01JB', name: 'Beta', failed: 4 }),
    ];
    expect(boardsNeedingAttention(boards)).toBe(2);
    expect(boardsNeedingAttention([])).toBe(0);
  });
});

describe('attentionFor', () => {
  it('finds one board and reports nothing for the rest', () => {
    const boards = [board({ held: 1 })];
    expect(attentionFor(boards, '01JA')?.held).toBe(1);
    expect(attentionFor(boards, '01JQUIET')).toBeNull();
  });
});

describe('attentionSummary', () => {
  it('says what is waiting, naming the board when there is only one', () => {
    expect(attentionSummary([board({ approvals: 1, failed: 2 })])).toBe(
      'Alpha: 1 waiting on approval, 2 failed',
    );
  });

  it('sums across boards and says how many there are', () => {
    const summary = attentionSummary([
      board({ held: 1 }),
      board({ project_id: '01JB', name: 'Beta', held: 2, failed: 1 }),
    ]);
    expect(summary).toBe('2 boards: 3 held on budget, 1 failed');
  });

  it('names the unread signal separately from the actionable ones', () => {
    expect(attentionSummary([board({ unread: 3 })])).toBe(
      'Alpha: 3 new since you looked',
    );
  });

  it('omits the kinds that are at zero', () => {
    expect(attentionSummary([board({ held: 1 })])).toBe('Alpha: 1 held on budget');
  });

  it('says so plainly when nothing is waiting', () => {
    expect(attentionSummary([])).toBe('Nothing is waiting on you');
  });
});
