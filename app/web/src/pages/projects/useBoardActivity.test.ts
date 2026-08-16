import { describe, expect, it } from 'vitest';

import { activityFor, burnIsNearLimit, type BoardActivity } from './useBoardActivity';

function board(
  projectId: string,
  working: number,
  burnMicros: number,
  burnTokens = 0,
): BoardActivity {
  return { project_id: projectId, working, burn_micros: burnMicros, burn_tokens: burnTokens };
}

describe('activityFor', () => {
  it('finds a board and reports nothing for one the server left out', () => {
    const boards = [board('a', 2, 1_840_000), board('b', 0, 2_960_000)];
    expect(activityFor(boards, 'a')?.working).toBe(2);
    // A board with neither runs nor spend is absent from the response, and
    // "absent" has to read as idle rather than as an error.
    expect(activityFor(boards, 'c')).toBeNull();
  });
});

describe('burnIsNearLimit', () => {
  it('warns only once the day is nearly spent', () => {
    expect(burnIsNearLimit(2_960_000, 3_000_000)).toBe(true);
    expect(burnIsNearLimit(3_000_000, 3_000_000)).toBe(true);
    expect(burnIsNearLimit(1_840_000, 5_000_000)).toBe(false);
  });

  it('never warns about a ceiling that does not exist', () => {
    // No budget is the default, and a board with no ceiling cannot be
    // close to one — colouring it orange would invent a limit.
    expect(burnIsNearLimit(9_999_999, null)).toBe(false);
    expect(burnIsNearLimit(9_999_999, undefined)).toBe(false);
  });

  it('does not divide by a paused board', () => {
    // `0` pauses the board rather than setting a ceiling of nothing, so the
    // ratio is meaningless and must not come back as Infinity.
    expect(burnIsNearLimit(0, 0)).toBe(false);
    expect(burnIsNearLimit(100, 0)).toBe(false);
  });

  it('reads a token ceiling the same way it reads a money one', () => {
    expect(burnIsNearLimit(96_000, 100_000)).toBe(true);
    expect(burnIsNearLimit(50_000, 100_000)).toBe(false);
  });
});
