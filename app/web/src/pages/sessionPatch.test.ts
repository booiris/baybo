import { describe, expect, it } from 'vitest';

import { applySessionPatch, applySessionUserMessage, bumpSessionToFront } from './ChatPage';
import { bucketSessions, withoutArchived } from './chat/sessionBuckets';
import type { SessionSummary } from './chat/types';

function row(id: string, overrides: Partial<SessionSummary> = {}): SessionSummary {
  return {
    session_id: id,
    created_at: '2026-07-20T00:00:00Z',
    last_active: '2026-07-20T00:00:00Z',
    unread: 0,
    archived: false,
    pinned: false,
    ...overrides,
  };
}

describe('applySessionPatch — archive', () => {
  it('keeps the row and flips the flag, so the list stops drawing it', () => {
    const next = applySessionPatch([row('a'), row('b')], 'a', { archived: true });
    // Kept in state (unlike `hidden`, which removes it) …
    expect(next.map((s) => s.session_id)).toEqual(['a', 'b']);
    // … and out of the list.
    expect(withoutArchived(next).map((s) => s.session_id)).toEqual(['b']);
  });

  it('restores the row from the sparse unarchive patch alone', () => {
    // `PUT …/archive` broadcasts `{ archived }` and nothing else. Had the
    // archive dropped the row, this patch would have nothing to land on and the
    // conversation would stay missing until the next full refetch.
    const archived = applySessionPatch([row('a', { archived: true })], 'a', { archived: false });
    expect(withoutArchived(archived).map((s) => s.session_id)).toEqual(['a']);
  });

  it('leaves the flag alone on a patch that does not carry it', () => {
    const next = applySessionPatch([row('a', { archived: true })], 'a', { title: 'Renamed' });
    expect(next[0].title).toBe('Renamed');
    expect(next[0].archived).toBe(true);
  });

  it('preserves cron grouping and the group pin across an unrelated patch', () => {
    const next = applySessionPatch(
      [row('f1', { cron_job_id: 'job-a', cron_job_title: 'Digest', cron_group_pinned: true })],
      'f1',
      { title: 'Morning digest' },
    );
    expect(next[0].cron_job_id).toBe('job-a');
    expect(next[0].cron_job_title).toBe('Digest');
    expect(next[0].cron_group_pinned).toBe(true);
  });

  it('still removes a hidden row outright', () => {
    expect(applySessionPatch([row('a'), row('b')], 'a', { hidden: true })).toHaveLength(1);
  });

  it('constructs an unarchived row from a Created patch', () => {
    const next = applySessionPatch([], 'new', {
      created_at: '2026-07-20T01:00:00Z',
      last_active: '2026-07-20T01:00:00Z',
    });
    expect(next[0].archived).toBe(false);
  });
});

describe('bumpSessionToFront', () => {
  it('moves the row to the head and keeps everyone else in order', () => {
    const next = bumpSessionToFront([row('a'), row('b'), row('c'), row('d')], 'c');
    expect(next.map((s) => s.session_id)).toEqual(['c', 'a', 'b', 'd']);
  });

  it('returns the same array when there is nothing to move', () => {
    const prev = [row('a'), row('b')];
    // Already first, and absent — both must be identity, or React re-renders
    // the sidebar on every ping for a conversation sitting at the top.
    expect(bumpSessionToFront(prev, 'a')).toBe(prev);
    expect(bumpSessionToFront(prev, 'nope')).toBe(prev);
  });

  it('carries every field across the move', () => {
    const moved = row('f1', {
      title: 'Digest',
      unread: 4,
      pinned: true,
      folder_id: 'fold-1',
      last_user_text: 'hi',
      cron_job_id: 'job-a',
      cron_job_title: 'Digest',
      cron_group_pinned: true,
    });
    expect(bumpSessionToFront([row('a'), moved], 'f1')[0]).toEqual(moved);
  });
});

describe('applySessionUserMessage', () => {
  it('refreshes the preview and lifts the conversation to the front', () => {
    const next = applySessionUserMessage([row('a'), row('b')], 'b', '  hello   there\n');
    expect(next.map((s) => s.session_id)).toEqual(['b', 'a']);
    expect(next[0].last_user_text).toBe('hello there');
  });

  it('lifts the conversation even when the text is identical to last time', () => {
    // The regression that decides the shape of this helper: an early return on
    // an unchanged preview would leave the row wherever it was, so sending
    // "ok" twice into the same conversation would only raise it once.
    const prev = [row('a'), row('b', { last_user_text: 'ok' })];
    const next = applySessionUserMessage(prev, 'b', 'ok');
    expect(next.map((s) => s.session_id)).toEqual(['b', 'a']);
    expect(next[0].last_user_text).toBe('ok');
  });

  it('truncates the preview to the server\'s cap', () => {
    const next = applySessionUserMessage([row('a')], 'a', 'x'.repeat(200));
    expect(next[0].last_user_text).toBe(`${'x'.repeat(120)}…`);
  });

  it('is a no-op for an unknown session and for blank text', () => {
    const prev = [row('a')];
    expect(applySessionUserMessage(prev, 'nope', 'hi')).toBe(prev);
    expect(applySessionUserMessage(prev, 'a', '   ')).toBe(prev);
  });
});

describe('what the front of the list means, once bucketed', () => {
  const reachable = new Set(['fold-1']);

  it('lifts a pinned chat inside the Pinned block, not out of it', () => {
    const prev = [row('p1', { pinned: true }), row('p2', { pinned: true }), row('plain')];
    const buckets = bucketSessions(bumpSessionToFront(prev, 'p2'), reachable);
    expect(buckets.pinned.map((s) => s.session_id)).toEqual(['p2', 'p1']);
    expect(buckets.uncategorized.map((s) => s.session_id)).toEqual(['plain']);
  });

  it('lifts a foldered chat inside its own folder', () => {
    const prev = [
      row('f1', { folder_id: 'fold-1' }),
      row('f2', { folder_id: 'fold-1' }),
      row('loose'),
    ];
    const buckets = bucketSessions(bumpSessionToFront(prev, 'f2'), reachable);
    expect(buckets.chatsByFolder.get('fold-1')?.map((s) => s.session_id)).toEqual(['f2', 'f1']);
  });

  it('reorders a cron fire inside its group without floating the group', () => {
    // Group order is `last_active`, which only the server moves — see the
    // sort rationale in sessionBuckets.ts. Position alone must not reshuffle
    // the cron block.
    const fire = (id: string, job: string, at: string) =>
      row(id, { cron_job_id: job, cron_job_title: job, last_active: at });
    const prev = [
      fire('b1', 'job-b', '2026-07-20T03:00:00Z'),
      fire('a1', 'job-a', '2026-07-20T02:00:00Z'),
      fire('a2', 'job-a', '2026-07-20T01:00:00Z'),
    ];
    const buckets = bucketSessions(bumpSessionToFront(prev, 'a2'), reachable);
    expect(buckets.cronGroups.map((g) => g.jobId)).toEqual(['job-b', 'job-a']);
    expect(buckets.cronGroups[1].sessions.map((s) => s.session_id)).toEqual(['a2', 'a1']);
  });
});

describe('what must not reorder', () => {
  it('leaves the order alone on a field patch', () => {
    const next = applySessionPatch([row('a'), row('b')], 'b', { title: 'Renamed' });
    expect(next.map((s) => s.session_id)).toEqual(['a', 'b']);
  });
});
