import type { SessionSummary } from './types';

// How the sidebar splits the flat, newest-first session list into the blocks
// it renders: the lifted Pinned block, the derived cron groups, the per-folder
// buckets, and the trailing Uncategorized bucket.
//
// Kept out of the component so the rules are testable without a DOM: every row
// lands in exactly one bucket, and the precedence between pin / cron / folder
// is the whole feature.

/** Collapse-state keys are shared with folders (one localStorage set, see
 *  `folderStore`), so a cron group's key is namespaced — a cron job id and a
 *  folder id are different id spaces and must not alias. */
const CRON_COLLAPSE_PREFIX = 'cron:';

export function cronCollapseKey(jobId: string): string {
  return `${CRON_COLLAPSE_PREFIX}${jobId}`;
}

/**
 * Does this key belong to a group that is collapsed until told otherwise?
 *
 * Cron groups are: a half-hourly job opens 48 conversations a day, and expanded
 * by default it buries every real chat under its own fires. Folders are the
 * opposite — the user made one, so it opens showing what is in it.
 *
 * The stored set therefore records the **deviation** from a kind's default, not
 * an absolute state: membership means collapsed for a folder and expanded for a
 * cron group. Everything that reads or writes it must go through this predicate,
 * or the two meanings drift.
 */
export function collapsedByDefault(key: string): boolean {
  return key.startsWith(CRON_COLLAPSE_PREFIX);
}

/** Resolve a group's actual state from the stored deviation set.
 *
 *  Named to avoid `isCollapsed` on purpose: that is exactly what a caller names
 *  its local boolean, and a shadowed import that is missed by a rename reads as
 *  a function — truthy, type-checking, and rendering nothing. This repo has no
 *  eslint and no component render tests, so the compiler is the only guard. */
export function resolveCollapsed(deviations: ReadonlySet<string>, key: string): boolean {
  return deviations.has(key) !== collapsedByDefault(key);
}

/** A cron group: every visible fire of one recurring cron job, collapsed into
 *  a single labelled block. It is **derived**, never stored — not a folder, not
 *  a `session_folders` row, no id of its own beyond the job's. See
 *  `docs/cron-groups.md`. */
export interface CronGroup {
  jobId: string;
  /** The group's label (the job's live title, or its snapshot once the job is
   *  gone). A fire with no resolvable title is not groupable and stays flat. */
  title: string;
  /** Whether the user pinned this group (`cron_jobs.pinned`, surfaced on every
   *  member as `cron_group_pinned`). Pinned groups sort ahead of the rest of the
   *  cron block — the group is the only thing that can be pinned here, since a
   *  fire is a moment and the job is the recurring thing the user cares about. */
  pinned: boolean;
  /** Members drawn inside the group, in incoming (newest-first) order. */
  sessions: SessionSummary[];
}

export interface SessionBuckets {
  pinned: SessionSummary[];
  /** Newest-group-first (by each group's newest member's `last_active`). */
  cronGroups: CronGroup[];
  chatsByFolder: Map<string, SessionSummary[]>;
  uncategorized: SessionSummary[];
}

function newestActive(sessions: SessionSummary[]): number {
  let newest = Number.NEGATIVE_INFINITY;
  for (const s of sessions) {
    const t = new Date(s.last_active).getTime();
    if (!Number.isNaN(t) && t > newest) newest = t;
  }
  return newest;
}

/** Bucket the sidebar's sessions. Precedence, and every row lands in exactly
 *  one bucket:
 *
 *  1. `pinned` lifts the row out of everything else (a pinned cron fire escapes
 *     to the Pinned block rather than rendering inside its group — it would
 *     otherwise appear twice).
 *  2. A cron conversation (`cron_job_id`) groups by its job, and its
 *     `folder_id` is **ignored** — a fire can never be in a cron group and a
 *     user folder at once. A fire with no `cron_job_title` cannot be labelled,
 *     so it stays flat (Uncategorized) rather than inventing a name.
 *  3. Everything else buckets by `folder_id`, falling back to Uncategorized
 *     when that folder isn't reachable in the rendered tree.
 *
 *  A group with no visible members is never emitted, so an empty cron group
 *  cannot exist. Hidden rows never reach here (the list drops them).
 */
export function bucketSessions(
  sessions: SessionSummary[],
  reachableFolderIds: ReadonlySet<string>,
): SessionBuckets {
  const pinned: SessionSummary[] = [];
  const byJob = new Map<string, CronGroup>();
  const chatsByFolder = new Map<string, SessionSummary[]>();
  const uncategorized: SessionSummary[] = [];

  for (const s of sessions) {
    if (s.pinned) {
      pinned.push(s);
      continue;
    }
    if (s.cron_job_id) {
      if (s.cron_job_title) {
        const group = byJob.get(s.cron_job_id) ?? {
          jobId: s.cron_job_id,
          title: s.cron_job_title,
          // Every member of a job carries the same bit (it is read off the one
          // live job), so the first member to arrive settles it.
          pinned: s.cron_group_pinned ?? false,
          sessions: [],
        };
        group.sessions.push(s);
        byJob.set(s.cron_job_id, group);
      } else {
        uncategorized.push(s);
      }
      continue;
    }
    if (s.folder_id && reachableFolderIds.has(s.folder_id)) {
      const arr = chatsByFolder.get(s.folder_id) ?? [];
      arr.push(s);
      chatsByFolder.set(s.folder_id, arr);
    } else {
      uncategorized.push(s);
    }
  }

  // Folders carry a user-chosen `position`; a cron group has none, so it sorts
  // by its newest visible member — when a job fires, its group floats to the
  // top of the cron block. Ties fall back to the job id so the order is stable.
  //
  // A PINNED group sorts ahead of all of them. That is the only thing the pin
  // buys: a job that fires often is already at the top of this block by recency
  // ("48 fires a day become one row moving"), so the pin exists for the LOW
  // frequency job — the weekly digest that would otherwise sink between fires.
  const cronGroups = [...byJob.values()].sort((a, b) => {
    if (a.pinned !== b.pinned) return a.pinned ? -1 : 1;
    const diff = newestActive(b.sessions) - newestActive(a.sessions);
    return diff !== 0 ? diff : a.jobId.localeCompare(b.jobId);
  });

  return { pinned, cronGroups, chatsByFolder, uncategorized };
}

/** Unread across the members drawn *inside* the group. An escaped (pinned) fire
 *  is deliberately not counted — it carries its own badge in the Pinned block,
 *  and counting it here would double it. */
export function cronGroupUnread(group: CronGroup): number {
  return group.sessions.reduce((sum, s) => sum + s.unread, 0);
}
