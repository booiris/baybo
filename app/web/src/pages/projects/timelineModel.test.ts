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
  it('says record-only when nobody is on the issue', () => {
    expect(commentHint({ status: 'in_progress', assignee: null })).toContain('nobody is assigned');
  });

  it('says record-only for parked work even when it has an assignee', () => {
    expect(commentHint({ status: 'backlog', assignee: 'dev-1' })).toContain('not working on this');
    expect(commentHint({ status: 'done', assignee: 'dev-1' })).toContain('not working on this');
  });

  it('does not promise a wake that is not built', () => {
    // The honest failure here is the expensive one: a person who believes
    // they are talking to an agent waits for an answer nobody will send.
    const hint = commentHint({ status: 'in_progress', assignee: 'dev-1' });
    expect(hint).toContain('not built yet');
  });
});

describe('eventTime', () => {
  it('shows a clock for today and a date for anything older', () => {
    const now = Date.parse('2026-08-05T12:00:00Z');
    expect(eventTime(Date.parse('2026-08-05T09:30:00Z'), now)).toMatch(/\d/);
    expect(eventTime(Date.parse('2026-07-01T09:30:00Z'), now)).not.toMatch(/:/);
  });
});
