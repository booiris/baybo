import type { IssueRun } from './boardModel';
import type { TranscriptRow } from '../ChatPage';

/// Cutting a card's conversation back into the runs that wrote it.
///
/// The panel opens on a SESSION. One agent's runs on a card share one — a
/// retry continues where the last attempt stopped rather than starting a new
/// thread (`session_run_to_continue`, `crates/project/src/runs.rs`) — so the
/// page a run's icon opens also holds the attempts before it. That is the
/// honest shape: attempt 3 usually only makes sense against what attempt 2
/// did. What the transcript itself does not carry is any mark of which run a
/// row belongs to, so the seam has to be found from the ledger's timestamps,
/// and this module is the one place that does it.

/// The runs that worked in one session, oldest attempt first.
///
/// The execution log hands them over newest-first, and a transcript reads
/// downwards — so the log's order is exactly backwards here.
export function runsInSession(runs: readonly IssueRun[], sessionId: string): IssueRun[] {
  return runs
    .filter((run) => run.session_id === sessionId)
    .slice()
    .sort((a, b) => a.attempt - b.attempt);
}

/// When a row happened, in epoch ms, or `null` when it will not say.
///
/// A work card carries no `createdAt` — it is a fold of several rows, and the
/// only instant it keeps is when its turn opened.
export function rowAtMs(row: TranscriptRow): number | null {
  if (row.kind === 'work') return row.workStartedAt ?? null;
  if (row.createdAt === undefined) return null;
  const at = Date.parse(row.createdAt);
  return Number.isNaN(at) ? null : at;
}

export type RunSlice = {
  /// The run these rows were written by, or `null` for rows that predate the
  /// first of them — a session whose older attempts have paged off the top.
  run: IssueRun | null;
  rows: TranscriptRow[];
};

/// Split a page of rows into one slice per run that claimed the session.
///
/// A row joins the last run that had already started when it happened. A row
/// that will not say when it happened stays with whatever it followed rather
/// than opening a slice of its own: the alternative is a stray divider in the
/// middle of a turn, drawn on the strength of a missing field.
///
/// **Only runs that actually said something get a slice.** A page window
/// holding the newest 200 rows of a card retried all week starts above most of
/// its attempts, and a header announcing a run with nothing under it is the
/// panel reporting an empty run rather than an unloaded one.
///
/// Apply this AFTER the adjacent-work fold, so a turn cut in half by the page
/// window is one card by the time it is placed.
export function splitRowsByRun(
  rows: readonly TranscriptRow[],
  runsAsc: readonly IssueRun[],
): RunSlice[] {
  const started = runsAsc.filter((run) => run.started_at_ms != null);
  const slices: RunSlice[] = [];
  let current: RunSlice | null = null;
  let next = 0;

  for (const row of rows) {
    const at = rowAtMs(row);
    let owner: IssueRun | null = null;
    if (at !== null) {
      while (next < started.length && (started[next].started_at_ms ?? 0) <= at) {
        owner = started[next];
        next += 1;
      }
    }
    if (owner !== null) {
      current = { run: owner, rows: [] };
      slices.push(current);
    }
    if (current === null) {
      current = { run: null, rows: [] };
      slices.push(current);
    }
    current.rows.push(row);
  }
  return slices;
}

