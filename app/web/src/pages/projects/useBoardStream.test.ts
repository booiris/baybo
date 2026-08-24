import { describe, expect, it } from 'vitest';

import type { Frame } from '../../api/chatWs';
import { wantsRefresh } from './useBoardStream';


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

  it('refreshes the board when somebody comments, because a card counts that', () => {
    // The board used to skip timeline frames on the grounds that it draws
    // no timeline. It draws an unread count now, and that count moves on
    // these frames and on no others — skipping them left the one number
    // saying "an agent is asking you something" stale for as long as the
    // operator kept looking at it.
    expect(wantsRefresh(changed({ scope: 'timeline', issue_number: 7 }), 'p1', null)).toBe(true);
    expect(wantsRefresh(changed({ scope: 'timeline', issue_number: 7 }), 'p1', 7)).toBe(true);
    expect(wantsRefresh(changed({ scope: 'timeline', issue_number: 8 }), 'p1', 7)).toBe(false);
  });

  it('still ignores another board entirely', () => {
    expect(wantsRefresh(changed({ scope: 'timeline', issue_number: 7 }), 'p2', null)).toBe(false);
  });

  it('refreshes a card only for its own number — but never ignores a board-wide change', () => {
    expect(wantsRefresh(changed({ scope: 'run', issue_number: 7 }), 'p1', 7)).toBe(true);
    expect(wantsRefresh(changed({ scope: 'run', issue_number: 8 }), 'p1', 7)).toBe(false);
    expect(wantsRefresh(changed({ scope: 'project' }), 'p1', 7)).toBe(true);
  });
});
