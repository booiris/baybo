import { describe, expect, it } from 'vitest';

import { actorLabel, commentHint, describeEvent, eventShape, eventTime } from './timelineModel';
import type { IssueEvent, IssueEventBody } from './timelineModel';

function entry(body: IssueEventBody, agent?: string): IssueEvent {
  return {
    id: '01J',
    number: 4,
    actor: agent ?? 'user',
    actor_is_agent: agent != null,
    body,
    created_at_ms: 0,
  };
}

describe('describeEvent', () => {
  it('names both columns of a move, in the board’s own words', () => {
    expect(describeEvent({ kind: 'moved', from: 'backlog', to: 'in_progress' })).toBe(
      'moved it from Backlog to In Progress',
    );
  });

  it('reads all four assignment cases as sentences, not as templates', () => {
    // The one a naive template gets wrong is unassignment: it produces
    // "assigned it to nobody".
    expect(describeEvent({ kind: 'assigned', to: 'dev-1' })).toBe('assigned it to @dev-1');
    expect(describeEvent({ kind: 'assigned', from: 'dev-1', to: 'dev-2' })).toBe(
      'reassigned it from @dev-1 to @dev-2',
    );
    expect(describeEvent({ kind: 'assigned', from: 'dev-1' })).toBe('unassigned @dev-1');
    expect(describeEvent({ kind: 'assigned' })).toBe('unassigned it');
  });

  it('carries a run’s failure reason, and omits the dash when there is none', () => {
    expect(
      describeEvent({ kind: 'run_settled', attempt: 3, status: 'failed', error: 'ran out' }),
    ).toBe('run #3 failed — ran out');
    expect(describeEvent({ kind: 'run_settled', attempt: 1, status: 'done' })).toBe('run #1 done');
  });

  it('distinguishes a reclaimed worktree from one that was left alone', () => {
    expect(describeEvent({ kind: 'worktree_reclaimed', branch_deleted: true })).toContain(
      'nothing was committed',
    );
    expect(describeEvent({ kind: 'worktree_reclaimed', branch_deleted: false })).toContain(
      'branch is still there',
    );
    expect(
      describeEvent({ kind: 'worktree_kept', reason: 'contains modified or untracked files' }),
    ).toContain('contains modified or untracked files');
  });

  it('says nothing for a comment, because a comment is shown rather than narrated', () => {
    expect(describeEvent({ kind: 'comment', text: 'have a look' })).toBeNull();
    expect(eventShape(entry({ kind: 'comment', text: 'have a look' }))).toBe('comment');
    expect(eventShape(entry({ kind: 'opened' }))).toBe('note');
  });

  it('covers every kind — an unhandled one would return undefined', () => {
    const kinds: IssueEventBody[] = [
      { kind: 'opened' },
      { kind: 'moved', from: 'todo', to: 'done' },
      { kind: 'assigned', to: 'dev-1' },
      { kind: 'run_started', attempt: 1, trigger: 'started' },
      { kind: 'run_settled', attempt: 1, status: 'done' },
      { kind: 'blocked', reason: 'waiting' },
      { kind: 'unblocked' },
      { kind: 'cancelled' },
      { kind: 'worktree_reclaimed', branch_deleted: true },
      { kind: 'worktree_kept', reason: 'contains modified or untracked files' },
    ];
    for (const body of kinds) {
      expect(describeEvent(body), body.kind).toBeTypeOf('string');
    }
  });
});

describe('actorLabel', () => {
  it('calls the operator "you" and an agent by its handle', () => {
    expect(actorLabel(entry({ kind: 'opened' }))).toBe('you');
    expect(actorLabel(entry({ kind: 'opened' }, 'dev-1'))).toBe('@dev-1');
  });
});

describe('commentHint', () => {
  // Mirrors `comment_delivery` in crates/project — the same four branches,
  // asserted here so the two cannot drift silently.
  const live = [{ status: 'running' as const }];
  const queued = [{ status: 'queued' as const }];
  const settled = [{ status: 'done' as const }];

  it('says record-only when nobody is on the issue', () => {
    expect(commentHint({ status: 'in_progress', assignee: null }, [])).toContain(
      'nobody is assigned',
    );
  });

  it('says record-only for parked or cancelled work even with an assignee', () => {
    expect(commentHint({ status: 'backlog', assignee: 'dev-1' }, [])).toContain(
      'not working on this',
    );
    expect(commentHint({ status: 'done', assignee: 'dev-1' }, [])).toContain('not working on this');
    expect(
      commentHint({ status: 'in_progress', assignee: 'dev-1', cancelled_at_ms: 1 }, []),
    ).toContain('cancelled');
  });

  it('promises a run when the assignee is on live work and nothing is reading', () => {
    expect(commentHint({ status: 'todo', assignee: 'dev-1' }, [])).toBe(
      'Starts a run: @dev-1 will read this now.',
    );
    // A settled run leaves the issue idle again.
    expect(commentHint({ status: 'review', assignee: 'dev-1' }, settled)).toContain('Starts a run');
  });

  it('distinguishes a queued run from one already going', () => {
    // The two differ in when the comment is read, and that is exactly what
    // somebody about to press send wants to know.
    expect(commentHint({ status: 'in_progress', assignee: 'dev-1' }, queued)).toContain(
      'when the queued run starts',
    );
    expect(commentHint({ status: 'in_progress', assignee: 'dev-1' }, live)).toContain(
      'when that run finishes',
    );
  });
});

describe('eventTime', () => {
  it('shows a clock for today and a date for anything older', () => {
    const now = Date.parse('2026-08-05T12:00:00Z');
    expect(eventTime(Date.parse('2026-08-05T09:30:00Z'), now)).toMatch(/\d/);
    expect(eventTime(Date.parse('2026-07-01T09:30:00Z'), now)).not.toMatch(/:/);
  });
});
