import { describe, expect, it } from 'vitest';

import {
  actorLabel,
  approvalAsks,
  commentHint,
  describeEvent,
  eventShape,
  eventTime,
  eventTone,
  feedLine,
  feedTone,
} from './timelineModel';
import type { FeedEntry, IssueEvent, IssueEventBody } from './timelineModel';

const DEV_ID = '01JC3KQ4Z8AAAAAAAAAAAAAAAA';

function entry(body: IssueEventBody, actor: IssueEvent['actor'] = { kind: 'user' }): IssueEvent {
  return { id: '01J', number: 4, actor, body, created_at_ms: 0 };
}

function ref(handle: string) {
  return { id: DEV_ID, handle };
}

function agent(handle: string): IssueEvent['actor'] {
  return { kind: 'agent', ...ref(handle) };
}

describe('describeEvent', () => {
  it('names both columns of a move, in the board’s own words', () => {
    expect(describeEvent({ kind: 'moved', from: 'backlog', to: 'in_progress' })).toBe(
      'moved it from Backlog to In Progress',
    );
  });

  it('reads all four assignment cases as sentences, not as templates', () => {
    expect(describeEvent({ kind: 'assigned', to: ref('dev-1') })).toBe('assigned it to @dev-1');
    expect(describeEvent({ kind: 'assigned', from: ref('dev-1'), to: ref('dev-2') })).toBe(
      'reassigned it from @dev-1 to @dev-2',
    );
    expect(describeEvent({ kind: 'assigned', from: ref('dev-1') })).toBe('unassigned @dev-1');
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
      'nothing this repo did not already have',
    );
    expect(
      describeEvent({ kind: 'worktree_reclaimed', branch_deleted: true }),
    ).not.toContain('nothing was committed');
    expect(describeEvent({ kind: 'worktree_reclaimed', branch_deleted: false })).toContain(
      'branch is still there',
    );
    expect(
      describeEvent({ kind: 'worktree_kept', reason: 'contains modified or untracked files' }),
    ).toContain('contains modified or untracked files');
  });

  it('says a stage emptied without claiming anybody was woken', () => {
    const sentence = describeEvent({ kind: 'stage_completed', stage: 2 });
    expect(sentence).toBe('stage 2 finished — every step in it is done or called off');
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
      { kind: 'assigned', to: ref('dev-1') },
      { kind: 'run_started', attempt: 1, trigger: 'started' },
      { kind: 'run_settled', attempt: 1, status: 'done' },
      { kind: 'blocked', reason: 'waiting' },
      { kind: 'unblocked' },
      { kind: 'cancelled' },
      { kind: 'worktree_reclaimed', branch_deleted: true },
      { kind: 'worktree_kept', reason: 'contains modified or untracked files' },
      { kind: 'approval_requested', call_id: 'c1', tool: 'Bash', summary: 'rm -rf build' },
      { kind: 'approval_resolved', call_id: 'c1', decision: 'approve', resolution: 'answered' },
      { kind: 'stage_completed', stage: 1 },
      { kind: 'budget_exhausted', spent_micros: 5_000_000, limit_micros: 5_000_000 },
      { kind: 'budget_restored', spent_micros: 1_000_000, limit_micros: 5_000_000 },
    ];
    for (const body of kinds) {
      expect(describeEvent(body), body.kind).toBeTypeOf('string');
    }
  });

  it('narrates a body kind it has never heard of instead of dropping the entry', () => {
    const future = { kind: 'merged', sha: 'ab12cd' } as unknown as IssueEventBody;
    const sentence = describeEvent(future);
    expect(sentence).toBeTypeOf('string');
    expect(sentence).not.toBe('');
  });
});

describe('actorLabel', () => {
  it('calls the operator "you" and an agent by its handle — never by its id', () => {
    expect(actorLabel(entry({ kind: 'opened' }))).toBe('you');
    expect(actorLabel(entry({ kind: 'opened' }, agent('dev-1')))).toBe('@dev-1');
  });

  it('names an actor kind it has never heard of rather than rendering undefined', () => {
    const future = { kind: 'webhook', id: DEV_ID } as unknown as IssueEvent['actor'];
    expect(actorLabel(entry({ kind: 'opened' }, future))).toBe('somebody');
  });

  it('calls the board’s own gate "the board"', () => {
    expect(
      actorLabel(
        entry({ kind: 'budget_exhausted', spent_micros: 0, limit_micros: 0 }, { kind: 'system' }),
      ),
    ).toBe('the board');
  });
});

describe('commentHint', () => {
  // A real assignee is an agent **id**, not a handle. The fixture used to
  // be 'dev-1', which is handle-shaped — so a hint that printed the id
  // read correctly here while showing a raw ULID in the app.
  const DEV_1 = '01KZAD1QBS4A1XH456XJ7AC0V9';
  const team = [
    { id: DEV_1, handle: 'dev-1' },
  ] as unknown as Parameters<typeof commentHint>[2];
  const live = [{ status: 'running' as const }];
  const queued = [{ status: 'queued' as const }];
  const settled = [{ status: 'done' as const }];
  const held = [{ status: 'held' as const }];

  it('says record-only when nobody is on the issue', () => {
    const hint = commentHint({ status: 'in_progress', assignee: null }, [], team);
    expect(hint).toContain('nobody is assigned');
  });

  it('says record-only for parked or cancelled work even with an assignee', () => {
    expect(commentHint({ status: 'backlog', assignee: DEV_1 }, [], team)).toContain(
      'not working on this',
    );
    expect(commentHint({ status: 'done', assignee: DEV_1 }, [], team)).toContain(
      'not working on this',
    );
    expect(
      commentHint({ status: 'in_progress', assignee: DEV_1, cancelled_at_ms: 1 }, [], team),
    ).toContain('cancelled');
  });

  it('promises a run when the assignee is on live work and nothing is reading', () => {
    expect(commentHint({ status: 'todo', assignee: DEV_1 }, [], team)).toBe(
      'Starts a run: @dev-1 will read this now.',
    );
    // The id must never reach the composer — a person is addressed by
    // handle, and a ULID in a sentence is the tell that a raw row escaped.
    expect(commentHint({ status: 'todo', assignee: DEV_1 }, [], team)).not.toContain(DEV_1);
    expect(commentHint({ status: 'review', assignee: DEV_1 }, settled, team)).toContain(
      'Starts a run',
    );
  });

  it('distinguishes a queued run from one already going', () => {
    expect(commentHint({ status: 'in_progress', assignee: DEV_1 }, queued, team)).toContain(
      'when the queued run starts',
    );
    expect(commentHint({ status: 'in_progress', assignee: DEV_1 }, live, team)).toContain(
      'when that run finishes',
    );
  });

  it('says a block has stopped the issue, whatever is recorded against it', () => {
    for (const runs of [[], queued, live, held, settled]) {
      const hint = commentHint(
        { status: 'in_progress', assignee: DEV_1, blocked_reason: 'which goal wins?' },
        runs,
        team,
      );
      expect(hint).toContain('a block has stopped this issue');
      expect(hint).toContain('@dev-1');
      expect(hint).not.toContain('Starts a run');
    }
  });

  it('says a held run will read it, and why it has not started', () => {
    const hint = commentHint({ status: 'in_progress', assignee: DEV_1 }, held, team);
    expect(hint).toContain('held run starts');
    expect(hint).toContain('daily ceilings');
  });
});

describe('eventTone', () => {
  it('separates the outcomes a reader is scanning the rail for', () => {
    expect(eventTone({ kind: 'run_settled', attempt: 1, status: 'done' })).toBe('ok');
    expect(eventTone({ kind: 'run_settled', attempt: 1, status: 'failed' })).toBe('err');
    expect(eventTone({ kind: 'run_settled', attempt: 1, status: 'cancelled' })).toBe('muted');
    expect(eventTone({ kind: 'blocked', reason: 'waiting on the API' })).toBe('warn');
    expect(eventTone({ kind: 'approval_resolved', call_id: 'c', decision: 'deny', resolution: 'answered' })).toBe('err');
    expect(eventTone({ kind: 'approval_resolved', call_id: 'c', decision: 'approve', resolution: 'answered' })).toBe('ok');
    // Nobody saying no is not a refusal: an expired window warns, a prompt
    // that died with its run is background noise.
    expect(eventTone({ kind: 'approval_resolved', call_id: 'c', decision: 'deny', resolution: 'timed_out' })).toBe('warn');
    expect(eventTone({ kind: 'approval_resolved', call_id: 'c', decision: 'deny', resolution: 'abandoned' })).toBe('muted');
  });

  it('falls back to a neutral dot rather than throwing on an unknown entry', () => {
    expect(eventTone({ kind: 'opened' })).toBe('muted');
  });
});

describe('approvalAsks', () => {
  it('keeps the command of a settled prompt, so the frozen card can show it', () => {
    const ask = { kind: 'approval_requested' as const, call_id: 'c1', tool: 'Bash', summary: 'git push' };
    const asks = approvalAsks([
      { id: 'e1', number: 1, actor: { kind: 'user' }, body: ask, created_at_ms: 0 },
      {
        id: 'e2',
        number: 1,
        actor: { kind: 'user' },
        body: { kind: 'approval_resolved', call_id: 'c1', decision: 'approve', resolution: 'answered' },
        created_at_ms: 1,
      },
    ] as unknown as Parameters<typeof approvalAsks>[0]);
    expect(asks.get('c1')).toMatchObject({ tool: 'Bash', summary: 'git push' });
  });
});

describe('eventTime', () => {
  it('carries a clock at every age — the date is what drops off, not the time', () => {
    const now = Date.parse('2026-08-05T12:00:00Z');
    const today = eventTime(Date.parse('2026-08-05T09:30:00Z'), now);
    const older = eventTime(Date.parse('2026-07-01T09:30:00Z'), now);

    // Locale decides the separator and the 12/24-hour form, so the assertion
    // is on the shape: two clock parts either way, and a date only on the
    // entry that needs one to be placed.
    expect(today).toMatch(/\d{1,2}:\d{2}/);
    expect(older).toMatch(/\d{1,2}:\d{2}/);
    expect(older).toMatch(/Jul/);
    expect(today).not.toMatch(/Aug/);
  });
});

describe('feedLine', () => {
  function feed(body: FeedEntry['body'], number: number | null = 7): FeedEntry {
    return {
      ...(number == null ? {} : { number }),
      actor: { kind: 'agent', id: DEV_ID, handle: 'dev-1' },
      body,
      created_at_ms: 0,
    } as FeedEntry;
  }
  const said = (entry: FeedEntry) => feedLine(entry).map((span) => span.text).join('');
  const bold = (entry: FeedEntry) =>
    feedLine(entry)
      .filter((span) => span.strong === true)
      .map((span) => span.text);

  it('names the card, because a board-wide feed has no "it"', () => {
    // `describeEvent` writes for a pane that is one card, so it can say
    // "moved it to Review". Ten of those in a row name nothing.
    expect(describeEvent({ kind: 'moved', from: 'todo', to: 'in_progress' })).toContain('it');
    expect(said(feed({ kind: 'moved', from: 'todo', to: 'in_progress' }))).toBe(
      '@dev-1 moved #7 Todo → In Progress',
    );
    expect(said(feed({ kind: 'opened' }))).toBe('@dev-1 opened #7');
    expect(said(feed({ kind: 'blocked', reason: 'sandbox has no tmux' }))).toBe(
      '@dev-1 blocked #7: sandbox has no tmux',
    );
  });

  it('bolds who acted, which card, and how a run ended', () => {
    // What the eye lands on when the drawer is skimmed rather than read.
    expect(bold(feed({ kind: 'run_settled', attempt: 3, status: 'failed', error: 'boom' }))).toEqual(
      ['failed', '#7'],
    );
    expect(said(feed({ kind: 'run_settled', attempt: 3, status: 'failed', error: 'boom' }))).toBe(
      'run #3 failed on #7 — boom',
    );
    expect(
      bold(feed({ kind: 'assigned', from: null, to: { id: DEV_ID, handle: 'qa-2' } })),
    ).toEqual(['@dev-1', '@qa-2', '#7']);
  });

  it('carries what a settled run took and cost, and omits either when unpriced', () => {
    // Derived server-side over the run's own cost window — the feed is
    // handed the numbers rather than deriving a second opinion of them.
    const settled = {
      ...feed({ kind: 'run_settled', attempt: 1, status: 'done' }),
      duration_ms: 130_000,
      cost_micros: 40_000,
    } as FeedEntry;
    expect(said(settled)).toBe('run #1 done on #7 · 2m10s · $0.04');

    // Absent is not zero: a run nobody claimed has no window, and "0s · $0.00"
    // would report that as a run that finished instantly having spent nothing.
    expect(said(feed({ kind: 'run_settled', attempt: 2, status: 'cancelled' }))).toBe(
      'run #2 cancelled on #7',
    );
  });

  it('leaves the card out of the lines that have none', () => {
    // A hire is a fact about the board. So is running out of budget.
    const hire = feed({ kind: 'hired', agent: { id: DEV_ID, handle: 'tester' } }, null);
    expect(said(hire)).toBe('@dev-1 hired @tester');
    expect(feedTone(hire)).toBe('brand');
    expect(
      said(feed({ kind: 'budget_exhausted', spent_micros: 5_000_000, limit_micros: 5_000_000 }, null)),
    ).toContain('daily budget exhausted');
  });

  it('never pours a comment into the feed', () => {
    // An agent's run report is hundreds of words; it would bury every line
    // around it. The card's own timeline is where the text belongs.
    const report = 'Root cause confirmed. '.repeat(40);
    expect(said(feed({ kind: 'comment', text: report }))).toBe('@dev-1 commented on #7');
  });
});
