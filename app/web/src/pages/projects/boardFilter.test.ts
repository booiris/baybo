import { describe, expect, it } from 'vitest';

import type { Agent, Issue, IssueRun } from './boardModel';
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
    unread: 0,
    last_run_failed: false,
    approval_pending: false,
    opened_by_agent: false,
    pinned: false,
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
    expect(matches(cancelled, filter(), TEAM, null)).toBe(true);
    expect(matches(cancelled, filter({ showCancelled: false }), TEAM, null)).toBe(false);
  });

  it('finds a card by title substring, case-insensitively', () => {
    const card = issue(1, { title: 'Wire the Board' });
    expect(matches(card, filter({ text: 'wire' }), TEAM, null)).toBe(true);
    expect(matches(card, filter({ text: 'BOARD' }), TEAM, null)).toBe(true);
    expect(matches(card, filter({ text: 'parser' }), TEAM, null)).toBe(false);
  });

  it('finds a card by number, with or without the hash', () => {
    const card = issue(12, { title: 'nothing to match on' });
    expect(matches(card, filter({ text: '#12' }), TEAM, null)).toBe(true);
    expect(matches(card, filter({ text: '12' }), TEAM, null)).toBe(true);
    expect(matches(card, filter({ text: '1' }), TEAM, null)).toBe(false);
  });

  it('separates "anyone" from "nobody"', () => {
    const taken = issue(1, { assignee: 'id-dev-1' });
    const free = issue(2);
    expect(matches(taken, filter(), TEAM, null)).toBe(true);
    expect(matches(free, filter(), TEAM, null)).toBe(true);

    const unassigned = filter({ assignee: { kind: 'unassigned' } });
    expect(matches(taken, unassigned, TEAM, null)).toBe(false);
    expect(matches(free, unassigned, TEAM, null)).toBe(true);

    const dev = filter({ assignee: { kind: 'handle', handle: 'dev-1' } });
    expect(matches(taken, dev, TEAM, null)).toBe(true);
    expect(matches(free, dev, TEAM, null)).toBe(false);
  });

  it('matches an assignee by the handle the card renders', () => {
    const departed = issue(1, { assignee: 'id-gone' });
    expect(matches(departed, filter({ assignee: { kind: 'handle', handle: 'dev-1' } }), TEAM, null)).toBe(
      false,
    );
  });

  it('narrows to what is new, on the card count and nothing else', () => {
    expect(matches(issue(1), filter({ unreadOnly: true }), TEAM, null)).toBe(false);
    expect(matches(issue(1, { unread: 2 }), filter({ unreadOnly: true }), TEAM, null)).toBe(true);
    // A failed run is its own narrowing precisely because looking never
    // clears it; folding it in here would hand this filter a card it
    // cannot empty.
    expect(matches(issue(1, { last_run_failed: true }), filter({ unreadOnly: true }), TEAM, null)).toBe(
      false,
    );
  });

  it('narrows to blocked work', () => {
    expect(matches(issue(1), filter({ blockedOnly: true }), TEAM, null)).toBe(false);
    expect(
      matches(issue(1, { blocked_reason: 'waiting on review' }), filter({ blockedOnly: true }), TEAM, null),
    ).toBe(true);
  });

  it('narrows to work the daily ceiling has stopped', () => {
    // Held is the one narrowing whose fact is not on the card's row — it
    // comes off the board's live runs, through the same `runIndicator` the
    // card's own word reads, so the two cannot disagree about what held is.
    const card = issue(1);
    expect(matches(card, filter({ heldOnly: true }), TEAM, 'held')).toBe(true);
    expect(matches(card, filter({ heldOnly: true }), TEAM, 'queued')).toBe(false);
    expect(matches(card, filter({ heldOnly: true }), TEAM, 'running')).toBe(false);
    expect(matches(card, filter({ heldOnly: true }), TEAM, null)).toBe(false);
    // Off, it says nothing about any of them.
    expect(matches(card, filter(), TEAM, null)).toBe(true);
  });

  it('applies every clause together', () => {
    const card = issue(1, { title: 'parser', assignee: 'id-dev-1', blocked_reason: 'stuck' });
    const all = filter({
      text: 'parser',
      assignee: { kind: 'handle', handle: 'dev-1' },
      blockedOnly: true,
    });
    expect(matches(card, all, TEAM, null)).toBe(true);
    expect(matches({ ...card, blocked_reason: undefined }, all, TEAM, null)).toBe(false);
  });
});

describe('filterBoard', () => {
  it('narrows every column and leaves the input alone', () => {
    const board = emptyBoard();
    board.backlog = [issue(1, { title: 'keep' }), issue(2, { title: 'drop' })];
    board.todo = [issue(3, { title: 'keep', status: 'todo' })];
    const view = filterBoard(board, filter({ text: 'keep' }), TEAM, []);
    expect(view.backlog.map((i) => i.number)).toEqual([1]);
    expect(view.todo.map((i) => i.number)).toEqual([3]);
    expect(board.backlog).toHaveLength(2);
  });

  it('resolves each card’s run state from the board’s own runs', () => {
    const board = emptyBoard();
    board.backlog = [issue(1), issue(2), issue(3)];
    const runs: IssueRun[] = [
      { number: 1, agent_id: 'a', trigger: 'started', status: 'held', attempt: 1, created_at_ms: 0 },
      {
        number: 2,
        agent_id: 'a',
        trigger: 'started',
        status: 'queued',
        attempt: 1,
        created_at_ms: 0,
      },
    ];
    expect(filterBoard(board, filter({ heldOnly: true }), TEAM, runs).backlog.map((i) => i.number))
      .toEqual([1]);
    // Card #3 has no run at all, and a board handed no runs holds nothing —
    // both must read as "not held" rather than as unknown.
    expect(filterBoard(board, filter({ heldOnly: true }), TEAM, []).backlog).toHaveLength(0);
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
          unreadOnly: true,
          blockedOnly: true,
          showCancelled: false,
        }),
      ),
    ).toBe(5);
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
      filter({ unreadOnly: true }),
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
