import { describe, expect, it } from 'vitest';
import {
  bucketSessions,
  collapsedByDefault,
  cronCollapseKey,
  cronGroupUnread,
  resolveCollapsed,
} from './sessionBuckets';
import type { SessionSummary } from './types';

const JOB_A = 'job-a';
const JOB_B = 'job-b';
const FOLDER = 'folder-1';
const REACHABLE = new Set([FOLDER]);

function session(id: string, overrides: Partial<SessionSummary> = {}): SessionSummary {
  return {
    session_id: id,
    created_at: '2026-07-14T00:00:00Z',
    last_active: '2026-07-14T00:00:00Z',
    unread: 0,
    pinned: false,
    ...overrides,
  };
}

/** A listed cron fire: both fields together are what makes a row groupable. */
function fire(id: string, jobId: string, overrides: Partial<SessionSummary> = {}): SessionSummary {
  return session(id, { cron_job_id: jobId, cron_job_title: 'Morning digest', ...overrides });
}

describe('cron groups', () => {
  it('collapses every fire of one job into a single group, labelled by its title', () => {
    const { cronGroups, uncategorized } = bucketSessions(
      [fire('f2', JOB_A), fire('f1', JOB_A), session('user-chat')],
      REACHABLE,
    );
    expect(cronGroups).toHaveLength(1);
    expect(cronGroups[0].jobId).toBe(JOB_A);
    expect(cronGroups[0].title).toBe('Morning digest');
    // Incoming newest-first order is preserved inside the group.
    expect(cronGroups[0].sessions.map((s) => s.session_id)).toEqual(['f2', 'f1']);
    expect(uncategorized.map((s) => s.session_id)).toEqual(['user-chat']);
  });

  it('renders every row exactly once — a pinned fire escapes to the pinned block', () => {
    const { pinned, cronGroups } = bucketSessions(
      [fire('pinned-fire', JOB_A, { pinned: true }), fire('f1', JOB_A)],
      REACHABLE,
    );
    expect(pinned.map((s) => s.session_id)).toEqual(['pinned-fire']);
    expect(cronGroups[0].sessions.map((s) => s.session_id)).toEqual(['f1']);
  });

  it('ignores folder_id on a cron fire — it groups by its job, never by a folder', () => {
    const { cronGroups, chatsByFolder } = bucketSessions(
      [fire('f1', JOB_A, { folder_id: FOLDER }), session('c1', { folder_id: FOLDER })],
      REACHABLE,
    );
    expect(cronGroups[0].sessions.map((s) => s.session_id)).toEqual(['f1']);
    expect(chatsByFolder.get(FOLDER)?.map((s) => s.session_id)).toEqual(['c1']);
  });

  it('leaves an unnameable fire flat rather than inventing a group name', () => {
    const { cronGroups, uncategorized } = bucketSessions(
      [session('orphan', { cron_job_id: JOB_A, folder_id: FOLDER })],
      REACHABLE,
    );
    expect(cronGroups).toEqual([]);
    // Flat means uncategorized, not filed — folder_id stays ignored.
    expect(uncategorized.map((s) => s.session_id)).toEqual(['orphan']);
  });

  it('never emits a group with no visible members', () => {
    const { cronGroups } = bucketSessions(
      [fire('only-fire', JOB_A, { pinned: true })],
      REACHABLE,
    );
    expect(cronGroups).toEqual([]);
  });

  it('sorts groups by their newest visible member, so a firing job floats up', () => {
    const { cronGroups } = bucketSessions(
      [
        fire('a-old', JOB_A, { last_active: '2026-07-14T01:00:00Z' }),
        fire('b-new', JOB_B, { last_active: '2026-07-14T09:00:00Z', cron_job_title: 'Nightly' }),
        // A newer pinned member of A escapes, so it must not lift A's group.
        fire('a-pinned', JOB_A, { last_active: '2026-07-14T23:00:00Z', pinned: true }),
      ],
      REACHABLE,
    );
    expect(cronGroups.map((g) => g.jobId)).toEqual([JOB_B, JOB_A]);
  });

  it('sums unread only over the rows drawn inside the group', () => {
    const { cronGroups } = bucketSessions(
      [
        fire('f1', JOB_A, { unread: 3 }),
        fire('f2', JOB_A, { unread: 4 }),
        fire('f3', JOB_A, { unread: 9, pinned: true }),
      ],
      REACHABLE,
    );
    expect(cronGroupUnread(cronGroups[0])).toBe(7);
  });

  it('namespaces a group collapse key away from folder ids', () => {
    expect(cronCollapseKey(JOB_A)).not.toBe(JOB_A);
    expect(cronCollapseKey(JOB_A)).toBe('cron:job-a');
  });
});

describe('folder and pin bucketing (unchanged behaviour)', () => {
  it('lifts pinned rows out of the tree and falls unreachable folders back to uncategorized', () => {
    const { pinned, chatsByFolder, uncategorized } = bucketSessions(
      [
        session('p', { pinned: true, folder_id: FOLDER }),
        session('filed', { folder_id: FOLDER }),
        session('dangling', { folder_id: 'gone' }),
        session('loose'),
      ],
      REACHABLE,
    );
    expect(pinned.map((s) => s.session_id)).toEqual(['p']);
    expect(chatsByFolder.get(FOLDER)?.map((s) => s.session_id)).toEqual(['filed']);
    expect(uncategorized.map((s) => s.session_id)).toEqual(['dangling', 'loose']);
  });
});

describe('cron group pin', () => {
  it('sorts a pinned group ahead of a more recently fired one', () => {
    // JOB_B fired most recently, so by recency alone it would lead the block.
    // The pin is exactly what a LOW-frequency job needs: without it, the weekly
    // digest sinks under the half-hourly one forever.
    const { cronGroups } = bucketSessions(
      [
        fire('b1', JOB_B, { last_active: '2026-07-14T12:00:00Z' }),
        fire('a1', JOB_A, {
          last_active: '2026-07-01T00:00:00Z',
          cron_group_pinned: true,
        }),
      ],
      REACHABLE,
    );
    expect(cronGroups.map((g) => g.jobId)).toEqual([JOB_A, JOB_B]);
    expect(cronGroups[0].pinned).toBe(true);
    expect(cronGroups[1].pinned).toBe(false);
  });

  it('pins the GROUP without pinning its fires', () => {
    // The bit lives on the job, so no session's own `pinned` flips — and the
    // members must therefore stay INSIDE the group rather than escaping to the
    // Pinned block (which is what a session-level pin does).
    const { cronGroups, pinned } = bucketSessions(
      [fire('a1', JOB_A, { cron_group_pinned: true })],
      REACHABLE,
    );
    expect(pinned).toHaveLength(0);
    expect(cronGroups[0].sessions.map((s) => s.session_id)).toEqual(['a1']);
  });

  it('still lets an individually pinned fire escape a pinned group', () => {
    // The two pins are different things and both survive: the group holds its
    // seat at the top of the cron block, and the one fire the user pinned by
    // hand lifts out to the Pinned block. Each row is still rendered exactly
    // once — the escapee is not also drawn inside the group.
    const { cronGroups, pinned } = bucketSessions(
      [
        fire('a1', JOB_A, { cron_group_pinned: true, pinned: true }),
        fire('a2', JOB_A, { cron_group_pinned: true }),
      ],
      REACHABLE,
    );
    expect(pinned.map((s) => s.session_id)).toEqual(['a1']);
    expect(cronGroups[0].pinned).toBe(true);
    expect(cronGroups[0].sessions.map((s) => s.session_id)).toEqual(['a2']);
  });
});

describe('collapse defaults', () => {
  // The stored set records the DEVIATION from each kind's default, so the same
  // membership means opposite things for the two kinds. If this ever reads as
  // "the set of collapsed things", the cron default silently flips back.
  it('collapses cron groups by default and expands folders by default', () => {
    const none = new Set<string>();
    expect(resolveCollapsed(none, cronCollapseKey('job-a'))).toBe(true);
    expect(resolveCollapsed(none, 'folder-1')).toBe(false);
  });

  it('treats membership as the deviation, not the state', () => {
    const deviating = new Set([cronCollapseKey('job-a'), 'folder-1']);
    expect(resolveCollapsed(deviating, cronCollapseKey('job-a'))).toBe(false);
    expect(resolveCollapsed(deviating, 'folder-1')).toBe(true);
  });

  it('classifies by the cron key prefix, not by lookup', () => {
    expect(collapsedByDefault(cronCollapseKey('job-a'))).toBe(true);
    expect(collapsedByDefault('folder-1')).toBe(false);
    // A folder id that merely contains the word must not be mistaken for one.
    expect(collapsedByDefault('my-cron-folder')).toBe(false);
  });
})
