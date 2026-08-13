import { COLUMNS, emptyBoard, type Agent, type Board, type Issue } from './boardModel';
import { handleOf } from './teamModel';

export type AssigneeFilter =
  | { kind: 'anyone' }
  | { kind: 'unassigned' }
  | { kind: 'handle'; handle: string };

export type BoardFilter = {
  text: string;
  assignee: AssigneeFilter;
  blockedOnly: boolean;
  failedOnly: boolean;
  showCancelled: boolean;
};

/// Cancelled cards are **shown** by default, struck through, and filtered
/// out on request — not hidden and revealed on request. Cancel is the
/// terminal negative, not a delete: a card that vanishes the moment it is
/// called off takes its history off the board with it, and the operator
/// loses the one view that says what was dropped and when.
export const EMPTY_FILTER: BoardFilter = {
  text: '',
  assignee: { kind: 'anyone' },
  blockedOnly: false,
  failedOnly: false,
  showCancelled: true,
};

const PARAM_TEXT = 'q';
const PARAM_ASSIGNEE = 'assignee';
const PARAM_BLOCKED = 'blocked';
const PARAM_FAILED = 'failed';
const PARAM_CANCELLED = 'cancelled';

const UNASSIGNED = 'unassigned';

/// Every way the board can be narrowed, in one list. The menu's badge counts
/// these and its Clear button appears on them; written twice, the badge would
/// eventually say 0 next to a board that is hiding cards.
function restrictions(filter: BoardFilter): boolean[] {
  return [
    filter.text.trim() !== '',
    filter.assignee.kind !== 'anyone',
    filter.blockedOnly,
    filter.failedOnly,
    !filter.showCancelled,
  ];
}

export function restrictionCount(filter: BoardFilter): number {
  return restrictions(filter).filter(Boolean).length;
}

export function isDefault(filter: BoardFilter): boolean {
  return restrictionCount(filter) === 0;
}

function matchesText(issue: Issue, text: string): boolean {
  const needle = text.trim().toLowerCase();
  if (needle === '') return true;
  if (issue.title.toLowerCase().includes(needle)) return true;
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
      return issue.assignee != null && handleOf(team, issue.assignee) === filter.handle;
  }
}

export function matches(issue: Issue, filter: BoardFilter, team: Agent[]): boolean {
  if (!filter.showCancelled && issue.cancelled_at_ms != null) return false;
  if (filter.blockedOnly && issue.blocked_reason == null) return false;
  if (filter.failedOnly && !issue.last_run_failed) return false;
  if (!matchesAssignee(issue, filter.assignee, team)) return false;
  return matchesText(issue, filter.text);
}

export function filterBoard(board: Board, filter: BoardFilter, team: Agent[]): Board {
  const view = emptyBoard();
  for (const status of COLUMNS) {
    view[status] = board[status].filter((issue) => matches(issue, filter, team));
  }
  return view;
}

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
    failedOnly: params.get(PARAM_FAILED) === '1',
    // Absent means shown: hiding is the deliberate act, so it is the one
    // that has to be written down in the URL.
    showCancelled: params.get(PARAM_CANCELLED) !== '0',
  };
}

export function boardFilterParams(filter: BoardFilter): URLSearchParams {
  const params = new URLSearchParams();
  if (filter.text.trim() !== '') params.set(PARAM_TEXT, filter.text);
  if (filter.assignee.kind === 'unassigned') {
    params.set(PARAM_ASSIGNEE, UNASSIGNED);
  } else if (filter.assignee.kind === 'handle') {
    params.set(PARAM_ASSIGNEE, `@${filter.assignee.handle}`);
  }
  if (filter.blockedOnly) params.set(PARAM_BLOCKED, '1');
  if (filter.failedOnly) params.set(PARAM_FAILED, '1');
  if (!filter.showCancelled) params.set(PARAM_CANCELLED, '0');
  return params;
}
