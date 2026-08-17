import { describe, expect, it } from 'vitest';

import {
  attentionElsewhere,
  attentionFor,
  attentionSummary,
  needsAttention,
  type ProjectAttention,
} from './useAttention';

function board(overrides: Partial<ProjectAttention> = {}): ProjectAttention {
  return {
    project_id: '01JA',
    name: 'Alpha',
    approvals: 0,
    failed: 0,
    unread: 0,
    ...overrides,
  };
}

describe('needsAttention', () => {
  it('is a yes or a no, never a tally the rail cannot discharge', () => {
    const boards = [
      board({ approvals: 3, unread: 2 }),
      board({ project_id: '01JB', name: 'Beta', failed: 4 }),
    ];
    expect(needsAttention(boards)).toBe(true);
    expect(needsAttention([])).toBe(false);
  });
});

describe('attentionElsewhere', () => {
  it('ignores the board the operator is already on', () => {
    const boards = [board({ unread: 2 })];
    expect(attentionElsewhere(boards, '01JA')).toBe(false);
    expect(attentionElsewhere(boards, '01JB')).toBe(true);
  });

  it('counts every board when none is open', () => {
    expect(attentionElsewhere([board({ failed: 1 })], null)).toBe(true);
    expect(attentionElsewhere([], null)).toBe(false);
  });
});

describe('attentionFor', () => {
  it('finds one board and reports nothing for the rest', () => {
    const boards = [board({ failed: 1 })];
    expect(attentionFor(boards, '01JA')?.failed).toBe(1);
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
      board({ approvals: 1 }),
      board({ project_id: '01JB', name: 'Beta', approvals: 2, failed: 1 }),
    ]);
    expect(summary).toBe('2 boards: 3 waiting on approval, 1 failed');
  });

  it('names the unread signal separately from the actionable ones', () => {
    expect(attentionSummary([board({ unread: 3 })])).toBe(
      'Alpha: 3 new since you looked',
    );
  });

  it('omits the kinds that are at zero', () => {
    expect(attentionSummary([board({ failed: 1 })])).toBe('Alpha: 1 failed');
  });

  it('never speaks for a board the ceiling has merely stopped', () => {
    // A spent ceiling is a standing condition, not an event, and the server
    // no longer reports it here at all — the board says it in its own
    // header, beside the setting that lifts it. A board with nothing but a
    // held run is absent from this list entirely.
    expect(attentionSummary([])).not.toContain('held');
  });

  it('says so plainly when nothing is waiting', () => {
    expect(attentionSummary([])).toBe('Nothing is waiting on you');
  });
});
