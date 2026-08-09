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
  showCancelled: boolean;
};

export const EMPTY_FILTER: BoardFilter = {
  text: '',
  assignee: { kind: 'anyone' },
  blockedOnly: false,
  showCancelled: false,
};

const PARAM_TEXT = 'q';
const PARAM_ASSIGNEE = 'assignee';
const PARAM_BLOCKED = 'blocked';
const PARAM_CANCELLED = 'cancelled';

const UNASSIGNED = 'unassigned';

export function isRestrictive(filter: BoardFilter): boolean {
  return (
    filter.text.trim() !== '' || filter.assignee.kind !== 'anyone' || filter.blockedOnly
  );
}

export function isDefault(filter: BoardFilter): boolean {
  return !isRestrictive(filter) && !filter.showCancelled;
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
    showCancelled: params.get(PARAM_CANCELLED) === '1',
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
  if (filter.showCancelled) params.set(PARAM_CANCELLED, '1');
  return params;
}
