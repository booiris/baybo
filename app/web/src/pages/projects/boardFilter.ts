import { COLUMNS, emptyBoard, type Agent, type Board, type Issue } from './boardModel';
import { handleOf } from './teamModel';

/**
 * Who a filter is looking for.
 *
 * A tagged union rather than a nullable handle, because "anyone" and
 * "nobody" are different questions and a string that means both is a string
 * somebody eventually compares to `''`.
 */
export type AssigneeFilter =
  | { kind: 'anyone' }
  | { kind: 'unassigned' }
  | { kind: 'handle'; handle: string };

/** What the board is being narrowed to. */
export type BoardFilter = {
  /** Matched against the title and the `#number`. */
  text: string;
  assignee: AssigneeFilter;
  blockedOnly: boolean;
  /** An *include* toggle: cancelled cards are hidden by default. */
  showCancelled: boolean;
};

export const EMPTY_FILTER: BoardFilter = {
  text: '',
  assignee: { kind: 'anyone' },
  blockedOnly: false,
  showCancelled: false,
};

/** Query-string keys. Named once so the codec and any link agree. */
const PARAM_TEXT = 'q';
const PARAM_ASSIGNEE = 'assignee';
const PARAM_BLOCKED = 'blocked';
const PARAM_CANCELLED = 'cancelled';

/**
 * The `assignee` value meaning "nobody is on it" — the set a triage pass is
 * about.
 *
 * Safe to write bare: a handle is always rendered with a leading `@` in the
 * query string, and `@` is outside the handle grammar, so the sentinel and
 * a real handle can never collide.
 */
const UNASSIGNED = 'unassigned';

/** Whether this filter would hide anything at all. */
export function isRestrictive(filter: BoardFilter): boolean {
  return (
    filter.text.trim() !== '' || filter.assignee.kind !== 'anyone' || filter.blockedOnly
  );
}

/** Whether the filter differs from the default in any way, including the
 *  cancelled toggle — which is what a Clear button should react to. */
export function isDefault(filter: BoardFilter): boolean {
  return !isRestrictive(filter) && !filter.showCancelled;
}

function matchesText(issue: Issue, text: string): boolean {
  const needle = text.trim().toLowerCase();
  if (needle === '') return true;
  if (issue.title.toLowerCase().includes(needle)) return true;
  // `#12` and `12` both find #12. A number typed with the hash is the way
  // the card is written everywhere else on this page.
  const bare = needle.startsWith('#') ? needle.slice(1) : needle;
  return String(issue.number) === bare;
}

function matchesAssignee(issue: Issue, filter: AssigneeFilter, team: Agent[]): boolean {
  switch (filter.kind) {
    case 'anyone':
      return true;
    case 'unassigned':
      return issue.assignee == null;
    case 'handle':
      // Compared against what the card *renders*, so a departed agent's
      // cards are still reachable by the handle shown on them — `handleOf`
      // falls back to the id, and the picker offers only live teammates, so
      // the fallback simply never matches rather than matching wrongly.
      return issue.assignee != null && handleOf(team, issue.assignee) === filter.handle;
  }
}

/** Whether one card survives the filter. */
export function matches(issue: Issue, filter: BoardFilter, team: Agent[]): boolean {
  if (!filter.showCancelled && issue.cancelled_at_ms != null) return false;
  if (filter.blockedOnly && issue.blocked_reason == null) return false;
  if (!matchesAssignee(issue, filter.assignee, team)) return false;
  return matchesText(issue, filter.text);
}

/**
 * The board as rendered. Never fed back into a move request — the request
 * carries the destination column's whole membership, which only the
 * unfiltered board knows.
 */
export function filterBoard(board: Board, filter: BoardFilter, team: Agent[]): Board {
  const view = emptyBoard();
  for (const status of COLUMNS) {
    view[status] = board[status].filter((issue) => matches(issue, filter, team));
  }
  return view;
}

/**
 * Read a filter out of the query string.
 *
 * Never fails. A hand-edited or stale URL degrades key by key to the
 * default, because the alternative — refusing to render — turns a typo into
 * a board the operator cannot open.
 */
export function parseBoardFilter(params: URLSearchParams): BoardFilter {
  const raw = params.get(PARAM_ASSIGNEE)?.trim() ?? '';
  let assignee: AssigneeFilter = { kind: 'anyone' };
  if (raw === UNASSIGNED) {
    assignee = { kind: 'unassigned' };
  } else if (raw.startsWith('@') && raw.length > 1) {
    assignee = { kind: 'handle', handle: raw.slice(1) };
  }
  return {
    text: params.get(PARAM_TEXT) ?? '',
    assignee,
    blockedOnly: params.get(PARAM_BLOCKED) === '1',
    showCancelled: params.get(PARAM_CANCELLED) === '1',
  };
}

/**
 * Write a filter back. Keys at their default are omitted, so an unfiltered
 * board has a clean URL and a shared link carries only what was chosen.
 */
export function boardFilterParams(filter: BoardFilter): URLSearchParams {
  const params = new URLSearchParams();
  if (filter.text.trim() !== '') params.set(PARAM_TEXT, filter.text);
  if (filter.assignee.kind === 'unassigned') {
    params.set(PARAM_ASSIGNEE, UNASSIGNED);
  } else if (filter.assignee.kind === 'handle') {
    params.set(PARAM_ASSIGNEE, `@${filter.assignee.handle}`);
  }
  if (filter.blockedOnly) params.set(PARAM_BLOCKED, '1');
  if (filter.showCancelled) params.set(PARAM_CANCELLED, '1');
  return params;
}
