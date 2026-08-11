import { describe, expect, it } from 'vitest';

import type { Agent, Issue } from './boardModel';
import { emptyBoard } from './boardModel';
import {
  EMPTY_FILTER,
  boardFilterParams,
  filterBoard,
  isDefault,
  matches,
  restrictionCount,
  parseBoardFilter,
  type BoardFilter,
} from './boardFilter';

function issue(number: number, overrides: Partial<Issue> = {}): Issue {
  return {
    number,
    project_id: '01JPROJECT',
    title: `card ${number}`,
    description: '',
    status: 'backlog',
    priority: 'none',
    position: number,
    stage: 0,
    created_at_ms: 0,
    updated_at_ms: 0,
    ...overrides,
  };
}

const TEAM: Agent[] = [
  {
    id: 'id-dev-1',
    handle: 'dev-1',
    name: 'Dev',
    description: '',
    framework: 'baybo',
    lead: false,
    created_at_ms: 0,
  },
];

function filter(overrides: Partial<BoardFilter> = {}): BoardFilter {
  return { ...EMPTY_FILTER, ...overrides };
}

describe('matches', () => {
  it('shows cancelled cards until they are filtered out', () => {
    // Cancel is the terminal negative, not a delete: the card stays on the
    // board struck through, and hiding it is the deliberate act.
    const cancelled = issue(1, { cancelled_at_ms: 123 });
    expect(matches(cancelled, filter(), TEAM)).toBe(true);
    expect(matches(cancelled, filter({ showCancelled: false }), TEAM)).toBe(false);
  });

  it('finds a card by title substring, case-insensitively', () => {
    const card = issue(1, { title: 'Wire the Board' });
    expect(matches(card, filter({ text: 'wire' }), TEAM)).toBe(true);
    expect(matches(card, filter({ text: 'BOARD' }), TEAM)).toBe(true);
    expect(matches(card, filter({ text: 'parser' }), TEAM)).toBe(false);
  });

  it('finds a card by number, with or without the hash', () => {
    const card = issue(12, { title: 'nothing to match on' });
    expect(matches(card, filter({ text: '#12' }), TEAM)).toBe(true);
    expect(matches(card, filter({ text: '12' }), TEAM)).toBe(true);
    expect(matches(card, filter({ text: '1' }), TEAM)).toBe(false);
  });

  it('separates "anyone" from "nobody"', () => {
    const taken = issue(1, { assignee: 'id-dev-1' });
    const free = issue(2);
    expect(matches(taken, filter(), TEAM)).toBe(true);
    expect(matches(free, filter(), TEAM)).toBe(true);

    const unassigned = filter({ assignee: { kind: 'unassigned' } });
    expect(matches(taken, unassigned, TEAM)).toBe(false);
    expect(matches(free, unassigned, TEAM)).toBe(true);

    const dev = filter({ assignee: { kind: 'handle', handle: 'dev-1' } });
    expect(matches(taken, dev, TEAM)).toBe(true);
    expect(matches(free, dev, TEAM)).toBe(false);
  });

  it('matches an assignee by the handle the card renders', () => {
    const departed = issue(1, { assignee: 'id-gone' });
    expect(matches(departed, filter({ assignee: { kind: 'handle', handle: 'dev-1' } }), TEAM)).toBe(
      false,
    );
  });

  it('narrows to blocked work', () => {
    expect(matches(issue(1), filter({ blockedOnly: true }), TEAM)).toBe(false);
    expect(
      matches(issue(1, { blocked_reason: 'waiting on review' }), filter({ blockedOnly: true }), TEAM),
    ).toBe(true);
  });

  it('applies every clause together', () => {
    const card = issue(1, { title: 'parser', assignee: 'id-dev-1', blocked_reason: 'stuck' });
    const all = filter({
      text: 'parser',
      assignee: { kind: 'handle', handle: 'dev-1' },
      blockedOnly: true,
    });
    expect(matches(card, all, TEAM)).toBe(true);
    expect(matches({ ...card, blocked_reason: undefined }, all, TEAM)).toBe(false);
  });
});

describe('filterBoard', () => {
  it('narrows every column and leaves the input alone', () => {
    const board = emptyBoard();
    board.backlog = [issue(1, { title: 'keep' }), issue(2, { title: 'drop' })];
    board.todo = [issue(3, { title: 'keep', status: 'todo' })];
    const view = filterBoard(board, filter({ text: 'keep' }), TEAM);
    expect(view.backlog.map((i) => i.number)).toEqual([1]);
    expect(view.todo.map((i) => i.number)).toEqual([3]);
    expect(board.backlog).toHaveLength(2);
  });
});

describe('restrictionCount / isDefault', () => {
  it('treats hiding cancelled as narrowing, and showing them as the default', () => {
    const hidden = filter({ showCancelled: false });
    expect(restrictionCount(hidden)).toBe(1);
    expect(isDefault(hidden)).toBe(false);
    expect(isDefault(filter())).toBe(true);
  });

  it('ignores whitespace in the search box', () => {
    expect(restrictionCount(filter({ text: '   ' }))).toBe(0);
    expect(restrictionCount(filter({ text: ' a ' }))).toBe(1);
  });

  it('counts each narrowing separately, because the menu badge shows the number', () => {
    expect(
      restrictionCount(
        filter({
          text: 'parser',
          assignee: { kind: 'handle', handle: 'dev-1' },
          blockedOnly: true,
          showCancelled: false,
        }),
      ),
    ).toBe(4);
  });
});

describe('the URL codec', () => {
  it('round-trips every filter shape', () => {
    for (const f of [
      filter(),
      filter({ text: 'parser' }),
      filter({ assignee: { kind: 'unassigned' } }),
      filter({ assignee: { kind: 'handle', handle: 'dev-1' } }),
      filter({ blockedOnly: true, showCancelled: true }),
      // The hidden case is the one that actually writes a param now.
      filter({ showCancelled: false }),
      filter({
        text: 'x',
        assignee: { kind: 'handle', handle: 'qa-2' },
        blockedOnly: true,
        showCancelled: true,
      }),
    ]) {
      expect(parseBoardFilter(boardFilterParams(f))).toEqual(f);
    }
  });

  it('leaves an unfiltered board with a clean URL', () => {
    expect(boardFilterParams(filter()).toString()).toBe('');
  });

  it('degrades a hand-edited URL to the default rather than to an empty board', () => {
    const params = new URLSearchParams('assignee=%40&blocked=yes&cancelled=maybe');
    expect(parseBoardFilter(params)).toEqual(filter());
  });

  it('cannot confuse the unassigned sentinel with a handle', () => {
    const asHandle = filter({ assignee: { kind: 'handle', handle: 'unassigned' } });
    expect(boardFilterParams(asHandle).get('assignee')).toBe('@unassigned');
    expect(parseBoardFilter(boardFilterParams(asHandle))).toEqual(asHandle);
    expect(parseBoardFilter(new URLSearchParams('assignee=unassigned')).assignee).toEqual({
      kind: 'unassigned',
    });
  });
});
