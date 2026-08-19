import { describe, expect, it } from 'vitest';

import type { TranscriptRow } from '../ChatPage';
import type { IssueRun } from './boardModel';
import { rowAtMs, runsInSession, splitRowsByRun } from './runThread';

function run(attempt: number, overrides: Partial<IssueRun> = {}): IssueRun {
  return {
    number: 7,
    attempt,
    agent_id: 'dev-1',
    status: 'done',
    trigger: 'started',
    created_at_ms: 0,
    session_id: '01SESSION',
    ...overrides,
  };
}

function say(key: string, atMs: number): TranscriptRow {
  return { key, role: 'assistant', text: key, createdAt: new Date(atMs).toISOString() };
}

function work(key: string, startedAt: number): TranscriptRow {
  return { key, role: 'system', text: '', kind: 'work', workStartedAt: startedAt, steps: [] };
}

describe('runsInSession', () => {
  it('reads the log backwards, because a transcript reads downwards', () => {
    // The execution log hands runs over newest-first; the panel places them
    // against a page that starts at the oldest row on it.
    const log = [run(3), run(2), run(1)];
    expect(runsInSession(log, '01SESSION').map((r) => r.attempt)).toEqual([1, 2, 3]);
  });

  it('keeps only the runs that worked in this session', () => {
    // A card can hold two agents' runs, each with its own session, and a run
    // nobody claimed has none at all.
    const log = [run(3, { session_id: '01OTHER' }), run(2, { session_id: undefined }), run(1)];
    expect(runsInSession(log, '01SESSION').map((r) => r.attempt)).toEqual([1]);
  });
});

describe('rowAtMs', () => {
  it('times a work card by the turn it opened, since it carries no other', () => {
    expect(rowAtMs(work('w1', 5_000))).toBe(5_000);
    expect(rowAtMs(say('m1', 9_000))).toBe(9_000);
  });

  it('says nothing rather than guessing when the row is undated', () => {
    expect(rowAtMs({ key: 'm2', role: 'user', text: 'hi' })).toBeNull();
    expect(rowAtMs({ key: 'w2', role: 'system', text: '', kind: 'work' })).toBeNull();
  });
});

describe('splitRowsByRun', () => {
  it('cuts one session into the runs that wrote it', () => {
    const runs = [run(1, { started_at_ms: 1_000 }), run(2, { started_at_ms: 10_000 })];
    const rows = [say('m1', 1_500), work('w1', 2_000), say('m2', 11_000)];

    const slices = splitRowsByRun(rows, runs);
    expect(slices.map((s) => s.run?.attempt)).toEqual([1, 2]);
    expect(slices[0].rows.map((r) => r.key)).toEqual(['m1', 'w1']);
    expect(slices[1].rows.map((r) => r.key)).toEqual(['m2']);
  });

  it('keeps an undated row with what it followed instead of opening a slice', () => {
    // A divider drawn on the strength of a missing field would land in the
    // middle of a turn.
    const runs = [run(1, { started_at_ms: 1_000 }), run(2, { started_at_ms: 10_000 })];
    const rows = [say('m1', 1_500), { key: 'm2', role: 'assistant' as const, text: 'no date' }];

    const slices = splitRowsByRun(rows, runs);
    expect(slices).toHaveLength(1);
    expect(slices[0].rows.map((r) => r.key)).toEqual(['m1', 'm2']);
  });

  it('gives rows older than the first run a slice with no run behind them', () => {
    // The page window can start above the oldest attempt it still holds.
    const runs = [run(2, { started_at_ms: 10_000 })];
    const rows = [say('m1', 1_000), say('m2', 11_000)];

    const slices = splitRowsByRun(rows, runs);
    expect(slices.map((s) => s.run?.attempt ?? null)).toEqual([null, 2]);
    expect(slices[0].rows.map((r) => r.key)).toEqual(['m1']);
  });

  it('ignores a run the executor never claimed', () => {
    // No start means no instant to cut at — and no rows of its own either.
    const runs = [run(1, { started_at_ms: 1_000 }), run(2, { status: 'queued' })];
    const slices = splitRowsByRun([say('m1', 2_000)], runs);
    expect(slices.map((s) => s.run?.attempt)).toEqual([1]);
  });

  it('gives no header to a run whose rows are above the window', () => {
    // The newest 200 rows of a card retried all week start above most of its
    // attempts. A header with nothing under it reports an empty run, when
    // what actually happened is an unloaded one.
    const runs = [
      run(1, { started_at_ms: 1_000 }),
      run(2, { started_at_ms: 2_000 }),
      run(3, { started_at_ms: 3_000 }),
    ];
    const slices = splitRowsByRun([say('m1', 9_000), say('m2', 9_500)], runs);
    expect(slices.map((s) => s.run?.attempt)).toEqual([3]);
    expect(slices[0].rows).toHaveLength(2);
  });

  it('holds a single-run session in one slice', () => {
    const runs = [run(1, { started_at_ms: 1_000 })];
    const rows = [say('m1', 1_500), work('w1', 2_000), say('m2', 3_000)];
    expect(splitRowsByRun(rows, runs)).toHaveLength(1);
  });
});
