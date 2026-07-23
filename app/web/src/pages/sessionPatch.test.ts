import { describe, expect, it } from 'vitest';

import { applySessionPatch } from './ChatPage';
import { withoutArchived } from './chat/sessionBuckets';
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
