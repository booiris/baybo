import { describe, expect, it } from 'vitest';

import {
  type Board,
  type Issue,
  type Project,
  cardDragId,
  columnDropId,
  emptyBoard,
  groupByStatus,
  liveCount,
  moveCard,
  orderedNumbers,
  parseDragId,
  placementChanged,
  resolveDrop,
  resolveLanding,
} from './boardModel';

function issue(number: number, overrides: Partial<Issue> = {}): Issue {
  return {
    number,
    project_id: 'p',
    title: `issue ${number}`,
    description: '',
    status: 'backlog',
    priority: 'none',
    position: number,
    created_at_ms: 0,
    updated_at_ms: 0,
    ...overrides,
  };
}

function project(id: string): Project {
  return {
    id,
    name: id,
    description: '',
    workdir: `/tmp/${id}`,
    created_at_ms: 0,
    updated_at_ms: 0,
  };
}

function board(columns: Partial<Board>): Board {
  return { ...emptyBoard(), ...columns };
}

describe('groupByStatus', () => {
  it('files each card under its column, in position order', () => {
    const grouped = groupByStatus(
      [
        issue(3, { status: 'todo', position: 1 }),
        issue(1, { status: 'backlog', position: 0 }),
        issue(2, { status: 'todo', position: 0 }),
      ],
      false,
    );
    expect(grouped.backlog.map((i) => i.number)).toEqual([1]);
    expect(grouped.todo.map((i) => i.number)).toEqual([2, 3]);
    expect(grouped.review).toEqual([]);
  });

  it('hides cancelled cards unless they are asked for', () => {
    const issues = [issue(1), issue(2, { cancelled_at_ms: 123 })];
    expect(groupByStatus(issues, false).backlog.map((i) => i.number)).toEqual([1]);
    expect(groupByStatus(issues, true).backlog.map((i) => i.number)).toEqual([1, 2]);
  });
});

describe('liveCount', () => {
  it('counts outstanding work, never cancelled cards', () => {
    // A shown-but-cancelled card is on the board and still not work.
    expect(liveCount([issue(1), issue(2, { cancelled_at_ms: 1 }), issue(3)])).toBe(2);
  });
});

describe('parseDragId', () => {
  it('round-trips both namespaces and refuses anything else', () => {
    expect(parseDragId(cardDragId(7))).toEqual({ kind: 'card', number: 7 });
    expect(parseDragId(columnDropId('review'))).toEqual({ kind: 'column', status: 'review' });
    expect(parseDragId('col:nonsense')).toBeNull();
    expect(parseDragId('card:abc')).toBeNull();
    expect(parseDragId('7')).toBeNull();
  });
});

describe('resolveDrop', () => {
  const start = board({
    backlog: [issue(1), issue(2)],
    todo: [issue(3, { status: 'todo' })],
  });

  it('takes the slot of the card it was dropped on', () => {
    const drop = resolveDrop(start, cardDragId(2), cardDragId(1));
    expect(drop).toMatchObject({ status: 'backlog', index: 0 });
    expect(moveCard(start, drop!).backlog.map((i) => i.number)).toEqual([2, 1]);
  });

  it('appends when dropped on a column body — the only way into an empty column', () => {
    const drop = resolveDrop(start, cardDragId(1), columnDropId('review'));
    expect(drop).toMatchObject({ status: 'review', index: 0 });
    const next = moveCard(start, drop!);
    expect(next.review.map((i) => i.number)).toEqual([1]);
    expect(next.backlog.map((i) => i.number)).toEqual([2]);
    expect(next.review[0].status).toBe('review');
  });

  it('resolves to nothing when the drag ends where it began', () => {
    expect(resolveDrop(start, cardDragId(1), cardDragId(1))).toBeNull();
    expect(resolveDrop(start, cardDragId(1), null)).toBeNull();
    // #2 is already last in backlog: appending it there changes nothing.
    expect(resolveDrop(start, cardDragId(2), columnDropId('backlog'))).toBeNull();
  });

  it('refuses a drag whose card is not on the board', () => {
    expect(resolveDrop(start, cardDragId(99), cardDragId(1))).toBeNull();
    expect(resolveDrop(start, columnDropId('todo'), cardDragId(1))).toBeNull();
  });
});

describe('moveCard', () => {
  it('never leaves a copy behind in the source column', () => {
    const start = board({ backlog: [issue(1), issue(2)], done: [] });
    const next = moveCard(start, { status: 'done', index: 0, issue: issue(1) });
    expect(next.backlog.map((i) => i.number)).toEqual([2]);
    expect(next.done.map((i) => i.number)).toEqual([1]);
    // …and the input is untouched, so a rollback has something to roll back to.
    expect(start.backlog.map((i) => i.number)).toEqual([1, 2]);
  });

  it('clamps an out-of-range index rather than tearing a hole', () => {
    const start = board({ todo: [issue(1, { status: 'todo' })] });
    const next = moveCard(start, { status: 'todo', index: 99, issue: issue(1, { status: 'todo' }) });
    expect(next.todo.map((i) => i.number)).toEqual([1]);
  });
});

describe('orderedNumbers + placementChanged', () => {
  it('reports the destination contents a move request has to send', () => {
    const start = board({ backlog: [issue(1), issue(2), issue(3)] });
    const next = moveCard(start, { status: 'backlog', index: 0, issue: issue(3) });
    expect(orderedNumbers(next, 'backlog')).toEqual([3, 1, 2]);
  });

  it('spots a no-op so a drop that changed nothing costs no request', () => {
    const start = board({ backlog: [issue(1), issue(2)] });
    const moved = moveCard(start, { status: 'todo', index: 0, issue: issue(1) });
    expect(placementChanged(start, moved, 1)).toBe(true);

    const same = moveCard(start, { status: 'backlog', index: 0, issue: issue(1) });
    expect(placementChanged(start, same, 1)).toBe(false);
  });
});

describe('resolveLanding', () => {
  it('returns to the remembered board when it still exists', () => {
    const projects = [project('a'), project('b')];
    expect(resolveLanding(projects, 'b')).toEqual({ kind: 'go', id: 'b' });
  });

  it('falls back to the most recently touched when the memory is stale', () => {
    // The remembered project was archived out of the listing since last visit.
    const projects = [project('a'), project('b')];
    expect(resolveLanding(projects, 'gone')).toEqual({ kind: 'go', id: 'a' });
    expect(resolveLanding(projects, null)).toEqual({ kind: 'go', id: 'a' });
  });

  it('asks for a first project when there are none', () => {
    expect(resolveLanding([], 'anything')).toEqual({ kind: 'empty' });
  });
});
