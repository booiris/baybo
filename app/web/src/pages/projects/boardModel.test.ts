import { describe, expect, it } from 'vitest';

import {
  type Board,
  type Issue,
  type IssueRun,
  type Project,
  assignableAgents,
  updatedAgo,
  cardDragId,
  columnDropId,
  dropRejection,
  emptyBoard,
  groupByStatus,
  hasDeliverable,
  moveAnnouncement,
  liveCount,
  moveCard,
  orderedNumbers,
  parseDragId,
  placementChanged,
  retryRejection,
  runDuration,
  runIndicator,
  resolveDrop,
  resolveLanding,
  unsettledRun,
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
    stage: 0,
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
    max_parallel_issue_runs: 3,
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
      ]);
    expect(grouped.backlog.map((i) => i.number)).toEqual([1]);
    expect(grouped.todo.map((i) => i.number)).toEqual([2, 3]);
    expect(grouped.review).toEqual([]);
  });

  it('keeps every card, cancelled ones included', () => {
    const issues = [issue(1), issue(2, { cancelled_at_ms: 123 })];
    expect(groupByStatus(issues).backlog.map((i) => i.number)).toEqual([1, 2]);
  });
});

describe('liveCount', () => {
  it('counts outstanding work, never cancelled cards', () => {
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
    expect(drop).toMatchObject({ status: 'backlog', before: 1 });
    expect(moveCard(start, drop!).backlog.map((i) => i.number)).toEqual([2, 1]);
  });

  it('appends when dropped on a column body — the only way into an empty column', () => {
    const drop = resolveDrop(start, cardDragId(1), columnDropId('review'));
    expect(drop).toMatchObject({ status: 'review', before: null });
    const next = moveCard(start, drop!);
    expect(next.review.map((i) => i.number)).toEqual([1]);
    expect(next.backlog.map((i) => i.number)).toEqual([2]);
    expect(next.review[0].status).toBe('review');
  });

  it('resolves to nothing when the drag ends where it began', () => {
    expect(resolveDrop(start, cardDragId(1), cardDragId(1))).toBeNull();
    expect(resolveDrop(start, cardDragId(1), null)).toBeNull();
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
    const next = moveCard(start, { status: 'done', before: null, issue: issue(1) });
    expect(next.backlog.map((i) => i.number)).toEqual([2]);
    expect(next.done.map((i) => i.number)).toEqual([1]);
    expect(start.backlog.map((i) => i.number)).toEqual([1, 2]);
  });

  it('appends when the anchor is not in the destination column', () => {
    const start = board({ todo: [issue(1, { status: 'todo' }), issue(2, { status: 'todo' })] });
    const next = moveCard(start, {
      status: 'todo',
      before: 99,
      issue: issue(3, { status: 'todo' }),
    });
    expect(next.todo.map((i) => i.number)).toEqual([1, 2, 3]);
  });

  it('lands the card in front of its anchor', () => {
    const start = board({ backlog: [issue(1), issue(2), issue(3)] });
    const next = moveCard(start, { status: 'backlog', before: 2, issue: issue(3) });
    expect(next.backlog.map((i) => i.number)).toEqual([1, 3, 2]);
  });

  it('places the same drop consistently in a filtered and an unfiltered column', () => {
    const full = board({ backlog: [issue(1), issue(2, { cancelled_at_ms: 1 }), issue(3)] });
    const visible = board({ backlog: [issue(1), issue(3)] });
    const drop = { status: 'backlog' as const, before: 3, issue: issue(4) };
    expect(moveCard(visible, drop).backlog.map((i) => i.number)).toEqual([1, 4, 3]);
    expect(moveCard(full, drop).backlog.map((i) => i.number)).toEqual([1, 2, 4, 3]);
  });
});

describe('orderedNumbers + placementChanged', () => {
  it('reports the destination contents a move request has to send', () => {
    const start = board({ backlog: [issue(1), issue(2), issue(3)] });
    const next = moveCard(start, { status: 'backlog', before: 1, issue: issue(3) });
    expect(orderedNumbers(next, 'backlog')).toEqual([3, 1, 2]);
  });

  it('spots a no-op so a drop that changed nothing costs no request', () => {
    const start = board({ backlog: [issue(1), issue(2)] });
    const moved = moveCard(start, { status: 'todo', before: null, issue: issue(1) });
    expect(placementChanged(start, moved, 1)).toBe(true);

    const same = moveCard(start, { status: 'backlog', before: 2, issue: issue(1) });
    expect(placementChanged(start, same, 1)).toBe(false);
  });
});

function run(number: number, overrides: Partial<IssueRun> = {}): IssueRun {
  return {
    number,
    attempt: 1,
    agent_id: 'dev-1',
    status: 'queued',
    trigger: 'started',
    created_at_ms: 0,
    ...overrides,
  };
}

describe('runIndicator', () => {
  it('says what a card is doing, and nothing when it is doing nothing', () => {
    const active = [run(1, { status: 'running' }), run(2)];
    expect(runIndicator(active, 1)).toBe('running');
    expect(runIndicator(active, 2)).toBe('queued');
    expect(runIndicator(active, 3)).toBeNull();
    expect(runIndicator([], 1)).toBeNull();
  });
});

describe('unsettledRun', () => {
  it('counts every state that is not over — `held` included', () => {
    expect(unsettledRun([run(1, { status: 'held' })])?.status).toBe('held');
    expect(unsettledRun([run(1, { status: 'queued' })])?.status).toBe('queued');
    expect(unsettledRun([run(1, { status: 'running' })])?.status).toBe('running');
  });

  it('is null once the ledger is settled', () => {
    for (const status of ['done', 'failed', 'cancelled'] as const) {
      expect(unsettledRun([run(1, { status })])).toBeNull();
    }
    expect(unsettledRun([])).toBeNull();
  });

  it('picks the live row out of a card’s history', () => {
    const history = [
      run(1, { attempt: 1, status: 'failed' }),
      run(1, { attempt: 2, status: 'cancelled' }),
      run(1, { attempt: 3, status: 'held' }),
    ];
    expect(unsettledRun(history)?.attempt).toBe(3);
  });
});

describe('runDuration', () => {
  it('measures a finished run, and an unfinished one against now', () => {
    expect(runDuration(run(1, { started_at_ms: 0, settled_at_ms: 45_000 }), 99_000)).toBe('45s');
    expect(runDuration(run(1, { started_at_ms: 0, settled_at_ms: 125_000 }), 0)).toBe('2m05s');
    expect(runDuration(run(1, { started_at_ms: 10_000 }), 40_000)).toBe('30s');
    expect(runDuration(run(1), 40_000)).toBeNull();
  });
});

describe('retryRejection', () => {
  it('refuses a card the board has finished with, in the server’s own words', () => {
    expect(retryRejection(issue(1, { assignee: 'dev-1', cancelled_at_ms: 111 }))).toBe(
      'this issue was cancelled — reopen it before running it again',
    );
    expect(retryRejection(issue(1, { assignee: 'dev-1', status: 'done' }))).toBe(
      'this issue is done — move it back into the board before running it again',
    );
    expect(
      retryRejection(issue(1, { assignee: 'dev-1', status: 'review', cancelled_at_ms: 111 })),
    ).toBe('this issue was cancelled — reopen it before running it again');
  });

  it('refuses a card nobody is on, first — the answer the click would bring back', () => {
    expect(retryRejection(issue(1))).toBe('an issue with nobody on it cannot be run');
    expect(retryRejection(issue(1, { cancelled_at_ms: 111 }))).toBe(
      'an issue with nobody on it cannot be run',
    );
  });

  it('lets every live column run', () => {
    for (const status of ['backlog', 'todo', 'in_progress', 'review'] as const) {
      expect(retryRejection(issue(1, { assignee: 'dev-1', status }))).toBeNull();
    }
  });
});

describe('dropRejection', () => {
  it('refuses In Progress for work nobody is on', () => {
    const unclaimed = issue(1);
    // The copy names the column the card snapped back to, because the
    // bounce is instant and otherwise the operator has to hunt for it.
    const refusal = dropRejection(unclaimed, 'in_progress');
    expect(refusal).toContain('Assign an agent first');
    expect(refusal).toContain('Backlog');
    for (const status of ['backlog', 'todo', 'review', 'done'] as const) {
      expect(dropRejection(unclaimed, status)).toBeNull();
    }
    expect(dropRejection(issue(1, { assignee: 'dev-1' }), 'in_progress')).toBeNull();
  });
});

describe('assignableAgents', () => {
  it('keeps only agents that can host an issue session', () => {
    const agents = [
      { id: 'a', name: 'A', framework: 'baybo' },
      { id: 'b', name: 'B', framework: 'codex' },
      { id: 'c', name: 'C', framework: 'claude' },
    ] as Parameters<typeof assignableAgents>[0];
    expect(assignableAgents(agents).map((a) => a.id)).toEqual(['a']);
  });
});

describe('resolveLanding', () => {
  it('returns to the remembered board when it still exists', () => {
    const projects = [project('a'), project('b')];
    expect(resolveLanding(projects, 'b')).toEqual({ kind: 'go', id: 'b' });
  });

  it('falls back to the most recently touched when the memory is stale', () => {
    const projects = [project('a'), project('b')];
    expect(resolveLanding(projects, 'gone')).toEqual({ kind: 'go', id: 'a' });
    expect(resolveLanding(projects, null)).toEqual({ kind: 'go', id: 'a' });
  });

  it('asks for a first project when there are none', () => {
    expect(resolveLanding([], 'anything')).toEqual({ kind: 'empty' });
  });
});

describe('updatedAgo', () => {
  const now = Date.UTC(2026, 7, 6, 12, 0, 0);
  const ago = (ms: number) => updatedAgo(now - ms, now);

  it('reads as `now` inside the minute, not as 0m', () => {
    expect(ago(0)).toBe('now');
    expect(ago(59_000)).toBe('now');
  });

  it('steps up one unit at a time', () => {
    expect(ago(60_000)).toBe('1m');
    expect(ago(59 * 60_000)).toBe('59m');
    expect(ago(60 * 60_000)).toBe('1h');
    expect(ago(23 * 3_600_000)).toBe('23h');
    expect(ago(24 * 3_600_000)).toBe('1d');
    expect(ago(6 * 86_400_000)).toBe('6d');
    expect(ago(7 * 86_400_000)).toBe('1w');
  });

  it('never renders a negative age', () => {
    expect(updatedAgo(now + 5_000, now)).toBe('now');
  });
});

describe('moveAnnouncement', () => {
  const card = (over: Partial<Issue> = {}) => issue(12, over);

  it('announces only the drop that started an agent', () => {
    const started = card({ status: 'in_progress', assignee: '01JAGENT' });
    expect(moveAnnouncement(started, 'todo', 'dev-1')).toBe('Queued for @dev-1 — #12');
  });

  it('says nothing about a reorder or an ordinary column change', () => {
    // The board is the control surface and the timeline is the audit
    // trail. A toast on every move is a toast nobody reads, which costs
    // the one toast that matters.
    expect(moveAnnouncement(card({ status: 'todo' }), 'todo', 'dev-1')).toBeNull();
    expect(moveAnnouncement(card({ status: 'review' }), 'in_progress', 'dev-1')).toBeNull();
    expect(moveAnnouncement(card({ status: 'done' }), 'review', 'dev-1')).toBeNull();
  });

  it('says nothing when the card was already in progress', () => {
    // Re-ordering inside In Progress does not start anything: the run is
    // already going, and claiming otherwise would double-count it.
    const inside = card({ status: 'in_progress', assignee: '01JAGENT' });
    expect(moveAnnouncement(inside, 'in_progress', 'dev-1')).toBeNull();
  });

  it('says nothing when nobody is on the card', () => {
    // That drop bounces instead; the refusal is what speaks.
    expect(moveAnnouncement(card({ status: 'in_progress' }), 'todo', null)).toBeNull();
  });
});

describe('hasDeliverable', () => {
  it('is the same question as "did this card produce something"', () => {
    // A research card's deliverable is its report, so it never shows a
    // branch — and the branch column is only written once there is a
    // commit, which is why one field answers both.
    expect(hasDeliverable(issue(1))).toBe(false);
    expect(hasDeliverable(issue(1, { branch: '' }))).toBe(false);
    expect(hasDeliverable(issue(1, { branch: 'issue/1-thing' }))).toBe(true);
  });
});
