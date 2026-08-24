import { describe, expect, it } from 'vitest';

import {
  activityFor,
  burnState,
  startOfBudgetDay,
  type BoardActivity,
} from './useBoardActivity';

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

describe('burnState', () => {
  it('warns only once the day is nearly spent', () => {
    expect(burnState(2_960_000, 3_000_000)).toBe('near');
    expect(burnState(1_840_000, 5_000_000)).toBe('ok');
  });

  it('says over rather than near from the moment the ceiling is reached', () => {
    // The boundary is the gate's: `spent >= limit` holds the next run, so
    // exactly at the ceiling the board has already stopped.
    expect(burnState(3_000_000, 3_000_000)).toBe('over');
    // The board this was written for: six times past a 100k token ceiling,
    // every run on it held. "Close to the daily ceiling" was the one thing
    // it could not be.
    expect(burnState(601_902, 100_000)).toBe('over');
  });

  it('never warns about a ceiling that does not exist', () => {
    // No budget is the default, and a board with no ceiling cannot be
    // close to one — colouring it orange would invent a limit.
    expect(burnState(9_999_999, null)).toBe('ok');
    expect(burnState(9_999_999, undefined)).toBe('ok');
  });

  it('reads a paused board as stopped, not as untouched', () => {
    // `0` is the pause, and the gate treats it as exhausted from the first
    // second. The ratio is never taken, so nothing divides by it.
    expect(burnState(0, 0)).toBe('over');
    expect(burnState(100, 0)).toBe('over');
  });

  it('reads a token ceiling the same way it reads a money one', () => {
    expect(burnState(96_000, 100_000)).toBe('near');
    expect(burnState(50_000, 100_000)).toBe('ok');
  });
});

describe('startOfBudgetDay', () => {
  it('is UTC midnight, which is the day the gate measures', () => {
    // 2026-08-17T00:01:46Z — the minute this board re-held a run. Under the
    // reader's midnight at UTC+8 the window would have opened eight hours
    // earlier and counted spend the gate had already forgotten.
    const at = Date.parse('2026-08-17T00:01:46.000Z');
    expect(new Date(startOfBudgetDay(at)).toISOString()).toBe('2026-08-17T00:00:00.000Z');
    // Late in the UTC day, where a UTC+8 reader has already turned the page.
    const evening = Date.parse('2026-08-17T23:59:59.000Z');
    expect(new Date(startOfBudgetDay(evening)).toISOString()).toBe('2026-08-17T00:00:00.000Z');
    expect(startOfBudgetDay(startOfBudgetDay(at))).toBe(startOfBudgetDay(at));
  });
});
