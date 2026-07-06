import { describe, expect, it } from 'vitest';
import { INITIAL_CURSOR, advanceFromLive, advanceFromSync } from './syncCursor';

describe('sync cursor (max-wins + rebase-dirty)', () => {
  it('a baseline sync adopts next_cursor and stays clean', () => {
    const next = advanceFromSync(INITIAL_CURSOR, 7, false);
    expect(next).toEqual({ cursor: 7, rebaseDirty: false });
  });

  it('a difference sync advances max-wins and never regresses', () => {
    const at7 = { cursor: 7, rebaseDirty: false };
    expect(advanceFromSync(at7, 12, false).cursor).toBe(12);
    // A stale/lower next_cursor cannot move the cursor backward.
    expect(advanceFromSync(at7, 3, false).cursor).toBe(7);
    // A null next_cursor (empty session) leaves the cursor put.
    expect(advanceFromSync(at7, null, false).cursor).toBe(7);
  });

  it('a rebased page sets the dirty flag; a later non-rebased sync clears it', () => {
    const rebased = advanceFromSync({ cursor: 7, rebaseDirty: false }, 30, true);
    expect(rebased).toEqual({ cursor: 30, rebaseDirty: true });
    const cleared = advanceFromSync(rebased, 31, false);
    expect(cleared).toEqual({ cursor: 31, rebaseDirty: false });
  });

  it('live ordinals advance the cursor max-wins while clean', () => {
    const at7 = { cursor: 7, rebaseDirty: false };
    expect(advanceFromLive(at7, 9).cursor).toBe(9);
    expect(advanceFromLive(at7, 5).cursor).toBe(7);
    // From a null baseline the first live ordinal seeds it.
    expect(advanceFromLive(INITIAL_CURSOR, 4).cursor).toBe(4);
  });

  it('a rebase-dirty cursor is frozen against live advances (the leapfrog guard)', () => {
    const dirty = { cursor: 30, rebaseDirty: true };
    // A live final-reply ordinal above the cursor must NOT advance it — only a
    // sync next_cursor may, until one non-rebased sync completes.
    expect(advanceFromLive(dirty, 99)).toBe(dirty);
    expect(advanceFromLive(dirty, 99).cursor).toBe(30);
  });
});
