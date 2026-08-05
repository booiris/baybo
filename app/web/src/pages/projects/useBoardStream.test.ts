import { describe, expect, it } from 'vitest';

import type { Frame } from '../../api/chatWs';
import { wantsRefresh } from './useBoardStream';

// The board's live updates are session-less broadcasts: every connection
// sees every project's frames, so what a page ignores is as load-bearing
// as what it acts on.

function changed(overrides: Partial<Extract<Frame, { kind: 'project_changed' }>> = {}): Frame {
  return { kind: 'project_changed', project_id: 'p1', scope: 'board', ...overrides };
}

describe('wantsRefresh', () => {
  it('ignores every frame that is not a project change', () => {
    expect(wantsRefresh({ kind: 'deck_changed' }, 'p1', null)).toBe(false);
    expect(wantsRefresh({ kind: 'pong' }, 'p1', null)).toBe(false);
  });

  it('ignores another board entirely', () => {
    expect(wantsRefresh(changed({ project_id: 'p2' }), 'p1', null)).toBe(false);
    expect(wantsRefresh(changed({ project_id: 'p2', issue_number: 3 }), 'p1', 3)).toBe(false);
  });

  it('refreshes a board on anything about it', () => {
    expect(wantsRefresh(changed(), 'p1', null)).toBe(true);
    expect(wantsRefresh(changed({ scope: 'run', issue_number: 7 }), 'p1', null)).toBe(true);
    expect(wantsRefresh(changed({ scope: 'project' }), 'p1', null)).toBe(true);
  });

  it('refreshes a card only for its own number — but never ignores a board-wide change', () => {
    expect(wantsRefresh(changed({ scope: 'run', issue_number: 7 }), 'p1', 7)).toBe(true);
    expect(wantsRefresh(changed({ scope: 'run', issue_number: 8 }), 'p1', 7)).toBe(false);
    // No number means the whole board moved — an archive, a reorder —
    // which can still change what this card shows.
    expect(wantsRefresh(changed({ scope: 'project' }), 'p1', 7)).toBe(true);
  });
});
