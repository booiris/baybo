import type { components, paths } from '../../api/schema';

export type Issue = components['schemas']['IssueDto'];
export type Agent = components['schemas']['TeamMemberDto'];
// Runs have no point-lookup endpoint, so the generator inlines their shape
// rather than emitting a named component. Taking the type from the response
// keeps it pinned to the spec without inventing an endpoint to name it.
export type IssueRun =
  paths['/v1/projects/{project_id}/runs']['get']['responses'][200]['content']['application/json']['items'][number];
export type RunStatus = IssueRun['status'];
export type Project = components['schemas']['ProjectDto'];
export type IssueStatus = Issue['status'];
export type IssuePriority = Issue['priority'];

/** Board order, left to right. The set is fixed — see docs/todo/kanban.md. */
export const COLUMNS: readonly IssueStatus[] = [
  'backlog',
  'todo',
  'in_progress',
  'review',
  'done',
] as const;

export const COLUMN_LABEL: Record<IssueStatus, string> = {
  backlog: 'Backlog',
  todo: 'Todo',
  in_progress: 'In Progress',
  review: 'Review',
  done: 'Done',
};

export const PRIORITIES: readonly IssuePriority[] = [
  'urgent',
  'high',
  'medium',
  'low',
  'none',
] as const;

/** One column's cards, in order. */
export type Board = Record<IssueStatus, Issue[]>;

export function emptyBoard(): Board {
  return { backlog: [], todo: [], in_progress: [], review: [], done: [] };
}

/**
 * Split a flat listing into columns, dropping cancelled cards unless they
 * were asked for. Column order within each list follows `position`, which
 * the server keeps dense.
 */
export function groupByStatus(issues: Issue[], showCancelled: boolean): Board {
  const board = emptyBoard();
  for (const issue of issues) {
    if (!showCancelled && issue.cancelled_at_ms != null) continue;
    board[issue.status].push(issue);
  }
  for (const status of COLUMNS) {
    board[status].sort((a, b) =>
      a.position === b.position ? a.number - b.number : a.position - b.position,
    );
  }
  return board;
}

/**
 * What a column header counts: live work. A cancelled card is still on the
 * board when the filter is off, but it is not outstanding work, so it never
 * counts.
 */
export function liveCount(column: Issue[]): number {
  return column.filter((issue) => issue.cancelled_at_ms == null).length;
}

// dnd ids are namespaced because cards and columns share one DndContext:
// a bare number could collide with a column's own droppable id.
const CARD_PREFIX = 'card:';
const COLUMN_PREFIX = 'col:';

export function cardDragId(number: number): string {
  return `${CARD_PREFIX}${number}`;
}

export function columnDropId(status: IssueStatus): string {
  return `${COLUMN_PREFIX}${status}`;
}

export type DragTarget =
  | { kind: 'card'; number: number }
  | { kind: 'column'; status: IssueStatus }
  | null;

export function parseDragId(raw: string): DragTarget {
  if (raw.startsWith(CARD_PREFIX)) {
    const number = Number(raw.slice(CARD_PREFIX.length));
    return Number.isFinite(number) ? { kind: 'card', number } : null;
  }
  if (raw.startsWith(COLUMN_PREFIX)) {
    const status = raw.slice(COLUMN_PREFIX.length) as IssueStatus;
    return COLUMNS.includes(status) ? { kind: 'column', status } : null;
  }
  return null;
}

export function findIssue(board: Board, number: number): Issue | null {
  for (const status of COLUMNS) {
    const found = board[status].find((issue) => issue.number === number);
    if (found !== undefined) return found;
  }
  return null;
}

function statusOf(board: Board, number: number): IssueStatus | null {
  for (const status of COLUMNS) {
    if (board[status].some((issue) => issue.number === number)) return status;
  }
  return null;
}

/** Where a drop lands. `null` when the drag resolves to nothing. */
export type Drop = { status: IssueStatus; index: number; issue: Issue };

/**
 * Resolve a drag into a placement.
 *
 * Dropping onto a card means "take that card's slot"; dropping onto a
 * column's body means "append" — which is the only way to reach an empty
 * column, since it has no cards to collide with.
 */
export function resolveDrop(board: Board, activeId: string, overId: string | null): Drop | null {
  const active = parseDragId(activeId);
  if (active === null || active.kind !== 'card') return null;
  const issue = findIssue(board, active.number);
  if (issue === null) return null;
  if (overId === null) return null;

  const over = parseDragId(overId);
  if (over === null) return null;

  if (over.kind === 'column') {
    const from = statusOf(board, active.number);
    // Already the last card of that column: appending would be a no-op that
    // still costs a request.
    const target = board[over.status];
    if (from === over.status && target[target.length - 1]?.number === active.number) {
      return null;
    }
    const index = from === over.status ? target.length - 1 : target.length;
    return { status: over.status, index, issue };
  }

  if (over.number === active.number) return null;
  const overStatus = statusOf(board, over.number);
  if (overStatus === null) return null;
  const index = board[overStatus].findIndex((candidate) => candidate.number === over.number);
  return { status: overStatus, index, issue };
}

/** Apply a placement. Pure, so the optimistic move and its test agree. */
export function moveCard(board: Board, drop: Drop): Board {
  const next = emptyBoard();
  for (const status of COLUMNS) {
    next[status] = board[status].filter((issue) => issue.number !== drop.issue.number);
  }
  const moved: Issue = { ...drop.issue, status: drop.status };
  const clamped = Math.max(0, Math.min(drop.index, next[drop.status].length));
  next[drop.status].splice(clamped, 0, moved);
  return next;
}

/** The destination column's contents, in order — what a move request sends. */
export function orderedNumbers(board: Board, status: IssueStatus): number[] {
  return board[status].map((issue) => issue.number);
}

/**
 * Whether a board differs from another in a way worth a request. Dropping a
 * card back where it started should cost nothing.
 */
export function placementChanged(before: Board, after: Board, number: number): boolean {
  const beforeStatus = statusOf(before, number);
  const afterStatus = statusOf(after, number);
  if (beforeStatus === null || afterStatus === null) return false;
  if (beforeStatus !== afterStatus) return true;
  const beforeIndex = before[beforeStatus].findIndex((issue) => issue.number === number);
  const afterIndex = after[afterStatus].findIndex((issue) => issue.number === number);
  return beforeIndex !== afterIndex;
}

/**
 * What a card should say about its work, given the board's unfinished
 * runs. `null` means nothing is happening on it.
 *
 * Only the unfinished states reach a card: a finished run is history and
 * belongs in the issue's execution log, not on its face.
 */
export function runIndicator(
  activeRuns: IssueRun[],
  number: number,
): 'queued' | 'running' | null {
  const run = activeRuns.find((candidate) => candidate.number === number);
  if (run === undefined) return null;
  return run.status === 'running' ? 'running' : 'queued';
}

/** How long a run took, or has been going. */
export function runDuration(run: IssueRun, now: number): string | null {
  if (run.started_at_ms == null) return null;
  const end = run.settled_at_ms ?? now;
  const seconds = Math.max(0, Math.round((end - run.started_at_ms) / 1000));
  if (seconds < 60) return `${seconds}s`;
  return `${Math.floor(seconds / 60)}m${String(seconds % 60).padStart(2, '0')}s`;
}

/**
 * Why a drop is refused, or `null` when it is fine.
 *
 * In Progress means somebody is on it: a card in that column with no
 * assignee is work the board claims is happening and nobody is doing. The
 * server enforces the same rule, so this is the message, not the guard.
 */
export function dropRejection(issue: Issue, target: IssueStatus): string | null {
  if (target === 'in_progress' && issue.assignee == null) {
    return `#${issue.number} needs an assignee before it can start`;
  }
  return null;
}

/** Only baybo-framework agents can host an issue's session. */
export function assignableAgents(agents: Agent[]): Agent[] {
  return agents.filter((agent) => agent.framework === 'baybo');
}

/**
 * Which project the rail should open, given what is on the server and what
 * the browser remembers. The remembered id can name a project that has
 * since been archived or removed, so it is a hint, never an answer.
 */
export function resolveLanding(
  projects: Project[],
  rememberedId: string | null,
): { kind: 'empty' } | { kind: 'go'; id: string } {
  if (projects.length === 0) return { kind: 'empty' };
  if (rememberedId !== null && projects.some((project) => project.id === rememberedId)) {
    return { kind: 'go', id: rememberedId };
  }
  // The listing is newest-touched first, so the head is the best guess at
  // what the operator was last doing.
  return { kind: 'go', id: projects[0].id };
}
