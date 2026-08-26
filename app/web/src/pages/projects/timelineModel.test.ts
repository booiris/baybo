import { describe, expect, it } from 'vitest';

import {
  actorLabel,
  approvalAsks,
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

  it('records title and description edits as readable activity', () => {
    expect(describeEvent({ kind: 'title_changed', from: 'Before', to: 'After' })).toBe(
      'changed the title from “Before” to “After”',
    );
    expect(describeEvent({ kind: 'description_changed' })).toBe('edited the description');
  });

  it('reads all four assignment cases as sentences, not as templates', () => {
    expect(describeEvent({ kind: 'assigned', to: ref('dev-1') })).toBe('assigned it to @dev-1');
    expect(describeEvent({ kind: 'assigned', from: ref('dev-1'), to: ref('dev-2') })).toBe(
      'reassigned it from @dev-1 to @dev-2',
    );
    expect(describeEvent({ kind: 'assigned', from: ref('dev-1') })).toBe('unassigned @dev-1');
    expect(describeEvent({ kind: 'assigned' })).toBe('unassigned it');
  });

  it('says a swallowed run was not started, and which run has the card', () => {
    // The refusal is the dedupe guard working, but the write that implied
    // the run has already committed — the card names the new agent, or sits
    // in its new column — so the card has to say the run did not happen.
    // Named for the holder, because that is the half a reader can act on.
    expect(
      describeEvent({ kind: 'run_refused', trigger: 'assigned', attempt: 4 }),
    ).toBe('did not start a run (assigned) — run #4 still has this card');
    // A read failure still records the refusal; it just cannot name who.
    expect(describeEvent({ kind: 'run_refused', trigger: 'started' })).toBe(
      'did not start a run (moved to In Progress) — this card already had one in flight',
    );
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

  it('says what a card filed, on the card that filed it', () => {
    expect(describeEvent({ kind: 'filed', number: 13 })).toBe("filed #13 out of this card's work");
  });

  it('says nothing for a comment, because a comment is shown rather than narrated', () => {
    expect(describeEvent({ kind: 'comment', text: 'have a look' })).toBeNull();
    expect(eventShape(entry({ kind: 'comment', text: 'have a look' }))).toBe('comment');
    expect(eventShape(entry({ kind: 'opened' }))).toBe('note');
  });

  it('covers every kind — an unhandled one would return undefined', () => {
    const kinds: IssueEventBody[] = [
      { kind: 'opened' },
      { kind: 'title_changed', from: 'Before', to: 'After' },
      { kind: 'description_changed' },
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
      { kind: 'filed', number: 13 },
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
    // Warn, not the `default:` arm's muted. This switch has no `never` stop,
    // so a new kind compiles green and reads as background noise — which is
    // the wrong answer for a card that records a change nothing acted on.
    expect(eventTone({ kind: 'run_refused', trigger: 'assigned', attempt: 4 })).toBe('warn');
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
    expect(said(feed({ kind: 'title_changed', from: 'Before', to: 'After' }))).toBe(
      '@dev-1 changed the title of #7',
    );
    expect(said(feed({ kind: 'description_changed' }))).toBe(
      '@dev-1 edited the description of #7',
    );
  });

  it('names both cards when one filed the other, and bolds both', () => {
    expect(said(feed({ kind: 'filed', number: 13 }))).toBe('@dev-1 filed #13 out of #7');
    expect(bold(feed({ kind: 'filed', number: 13 }))).toEqual(['@dev-1', '#13', '#7']);
  });

  it('bolds who acted, which card, and how a run ended', () => {
    // What the eye lands on when the drawer is skimmed rather than read.
    // The actor on a run entry is the run's own agent, not the card's
    // assignee — which on a board where a review handover is a reassignment
    // are routinely different agents.
    expect(bold(feed({ kind: 'run_settled', attempt: 3, status: 'failed', error: 'boom' }))).toEqual(
      ['@dev-1', 'failed', '#7'],
    );
    expect(said(feed({ kind: 'run_settled', attempt: 3, status: 'failed', error: 'boom' }))).toBe(
      "@dev-1's run #3 failed on #7 — boom",
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
    expect(said(settled)).toBe("@dev-1's run #1 done on #7 · 2m10s · $0.04");

    // Absent is not zero: a run nobody claimed has no window, and "0s · $0.00"
    // would report that as a run that finished instantly having spent nothing.
    expect(said(feed({ kind: 'run_settled', attempt: 2, status: 'cancelled' }))).toBe(
      "@dev-1's run #2 cancelled on #7",
    );
  });

  it('names the card and the run holding it when a run was refused', () => {
    // `feedLine`'s switch also ends in a plain `default:`, so this line
    // exists at all only because it was written by hand — the compiler
    // would have accepted the entry vanishing from the feed.
    expect(said(feed({ kind: 'run_refused', trigger: 'assigned', attempt: 4 }))).toBe(
      '#7 did not start a run (assigned) — run #4 still has it',
    );
    // The actor is always the board here, so the line names the card, not
    // a who — and the card is what is bold, being the thing to press.
    expect(bold(feed({ kind: 'run_refused', trigger: 'assigned', attempt: 4 }))).toEqual(['#7']);
    expect(said(feed({ kind: 'run_refused', trigger: 'stage_barrier' }))).toBe(
      '#7 did not start a run (stage barrier) — it already had one',
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

describe('a branch that landed', () => {
  const merged: IssueEventBody = {
    kind: 'branch_merged',
    branch: 'issue/20-integration',
    into: 'master',
    commit: '7e1674e648b5252ef276216b30eab085d487c890',
    commits: 35,
  };

  // The board's whole point is work reaching the repository, so the entry
  // reads as an outcome rather than as the muted routine note the switch's
  // `default` arm would otherwise give a new kind.
  it('reads as an outcome, not as routine bookkeeping', () => {
    expect(eventTone(merged)).toBe('ok');
  });

  it('names where it went, because the repo may not be on its trunk', () => {
    const line = describeEvent(merged);
    expect(line).toContain('issue/20-integration');
    expect(line).toContain('master');
    expect(line).toContain('7e1674e6');
    expect(line).toContain('35 commits');
  });

  it('counts one commit as one', () => {
    const one = describeEvent({ ...merged, commits: 1 });
    expect(one).toContain('1 commit');
    expect(one).not.toContain('1 commits');
  });

  it('survives a merge git would not name a commit for', () => {
    expect(describeEvent({ ...merged, commit: '' })).not.toContain('as ');
  });
});
