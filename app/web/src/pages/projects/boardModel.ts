import type { components, paths } from '../../api/schema';

export type Issue = components['schemas']['IssueDto'];
export type Agent = components['schemas']['TeamMemberDto'];
// The generator inlines runs because they have no point endpoint.
export type IssueRun =
  paths['/v1/projects/{project_id}/runs']['get']['responses'][200]['content']['application/json']['items'][number];
export type RunStatus = IssueRun['status'];
/// A card's execution log with its totals. The server sums it, so the rail
/// and the rows can never disagree about what the card cost.
export type RunLog =
  paths['/v1/projects/{project_id}/issues/{number}/runs']['get']['responses'][200]['content']['application/json'];
export type Project = components['schemas']['ProjectDto'];
export type IssueStatus = Issue['status'];
export type IssuePriority = Issue['priority'];

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

/// The same five names, for a pill too narrow to spell them out. Kept beside
/// the long form so a column renamed in one place cannot keep its old name
/// in the other.
export const COLUMN_PILL_LABEL: Record<IssueStatus, string> = {
  backlog: 'BACKLOG',
  todo: 'TODO',
  in_progress: 'IN PROG',
  review: 'REVIEW',
  done: 'DONE',
};

/// The tone a status wears wherever it is shown as a pill — the step rows,
/// the rail's own picker, and that picker's panel. One table, so a column
/// cannot read as "in progress" in one place and as nothing in another.
export const STATUS_PILL: Record<IssueStatus, string> = {
  backlog: 'border-black/35 bg-canvas text-ink-soft',
  todo: 'border-black/35 bg-canvas text-ink',
  in_progress: 'border-black bg-brand/30 text-ink font-bold',
  review: 'border-info/45 bg-info/12 text-info',
  done: 'border-ok/50 bg-ok/15 text-ok font-bold',
};

/// The board header's action group — filter, lead chat, activity, settings.
/// One skin, because four buttons sitting in a row are read as a set and a
/// hand-copied class string is how a set stops being one.
export const HEADER_ACTION =
  'inline-flex items-center gap-1 border-2 border-black rounded-md px-2 py-0.5 font-mono text-[0.62rem] font-bold uppercase tracking-wider transition-colors';
export const HEADER_ACTION_ON = 'bg-brand text-ink';
/// The hover was missing entirely: these were the only buttons on the board
/// that gave no sign of being live until they were pressed.
export const HEADER_ACTION_OFF = 'bg-surface hover:bg-brand/25';
/// The same button with nothing left to do — Mark read on a board that
/// already reads zero. It stays in the group rather than disappearing:
/// pressing it is what empties it, and a control that vanishes under the
/// press that emptied it slides the next button into the one after that.
export const HEADER_ACTION_DEAD = 'bg-surface text-ink-soft opacity-50';

export const PRIORITIES: readonly IssuePriority[] = [
  'urgent',
  'high',
  'medium',
  'low',
  'none',
] as const;

export type Board = Record<IssueStatus, Issue[]>;

export function emptyBoard(): Board {
  return { backlog: [], todo: [], in_progress: [], review: [], done: [] };
}

export function groupByStatus(issues: Issue[]): Board {
  const board = emptyBoard();
  for (const issue of issues) {
    board[issue.status].push(issue);
  }
  for (const status of COLUMNS) {
    board[status].sort((a, b) =>
      a.position === b.position ? a.number - b.number : a.position - b.position,
    );
  }
  return board;
}

export function liveCount(column: Issue[]): number {
  return column.filter((issue) => issue.cancelled_at_ms == null).length;
}

/// What is waiting on the operator across the whole board — the sum the
/// header's Mark-read button carries. Counted over every card the board
/// holds, not over the filtered view, because that is what the press
/// clears: a number that only counted what the filter let through would
/// leave cards at zero on a board that still lights the rail.
export function unreadTotal(board: Board): number {
  return COLUMNS.reduce(
    (total, status) => board[status].reduce((sum, issue) => sum + issue.unread, 0) + total,
    0,
  );
}

/// Cards with something new on top, everything else in the board's own
/// order.
///
/// Applied **once, to the board the fetch produced**, rather than to the
/// rendered view: one order, so what is on screen is what a drag is resolved
/// in and what a move writes. Kept in the view it was a second order that
/// existed only while nothing was being dragged, and the swap between the
/// two landed in dnd-kit's own drag-start commit — a card changed slots
/// inside its column before the first `over` resolved, so a 4px twitch
/// posted a reorder of a column nobody had touched.
///
/// It writes no `position`, not even through a drag: the partition is
/// stable, so the order the operator dragged their cards into survives
/// inside each half and a card falls back into it as soon as it is read,
/// and a move sends [`persistedOrder`] — the stored order with one card
/// moved — rather than the order on screen.
///
/// A cancelled card is never lifted. Cancel is terminal, and floating a
/// struck-through card over live work because somebody spoke on it before it
/// was called off is the board arguing with itself.
export function hoistUnread(board: Board): Board {
  const isNew = (issue: Issue) => issue.unread > 0 && issue.cancelled_at_ms == null;
  const view = emptyBoard();
  for (const status of COLUMNS) {
    const column = board[status];
    view[status] = [...column.filter(isNew), ...column.filter((issue) => !isNew(issue))];
  }
  return view;
}

// Cards and columns share one DndContext, so their ids need namespaces.
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

export function statusOf(board: Board, number: number): IssueStatus | null {
  for (const status of COLUMNS) {
    if (board[status].some((issue) => issue.number === number)) return status;
  }
  return null;
}

export type Drop = { status: IssueStatus; before: number | null; issue: Issue };

export function resolveDrop(view: Board, activeId: string, overId: string | null): Drop | null {
  const active = parseDragId(activeId);
  if (active === null || active.kind !== 'card') return null;
  const issue = findIssue(view, active.number);
  if (issue === null) return null;
  if (overId === null) return null;

  const over = parseDragId(overId);
  if (over === null) return null;

  if (over.kind === 'column') {
    const from = statusOf(view, active.number);
    const target = view[over.status];
    if (from === over.status && target[target.length - 1]?.number === active.number) {
      return null;
    }
    return { status: over.status, before: null, issue };
  }

  if (over.number === active.number) return null;
  const overStatus = statusOf(view, over.number);
  if (overStatus === null) return null;
  const column = view[overStatus];
  const overIndex = column.findIndex((candidate) => candidate.number === over.number);
  const activeIndex = column.findIndex((candidate) => candidate.number === active.number);
  const anchorIndex = activeIndex !== -1 && activeIndex < overIndex ? overIndex + 1 : overIndex;
  return { status: overStatus, before: column[anchorIndex]?.number ?? null, issue };
}

export function moveCard(board: Board, drop: Drop): Board {
  const next = emptyBoard();
  for (const status of COLUMNS) {
    next[status] = board[status].filter((issue) => issue.number !== drop.issue.number);
  }
  const moved: Issue = { ...drop.issue, status: drop.status };
  const column = next[drop.status];
  const at =
    drop.before === null
      ? column.length
      : (() => {
          const found = column.findIndex((issue) => issue.number === drop.before);
          return found === -1 ? column.length : found;
        })();
  column.splice(at, 0, moved);
  return next;
}

/// The order a move sends: the destination column in its **stored** order,
/// with the card that moved lifted out and put back in front of the card it
/// was dropped in front of.
///
/// Deliberately not the order on screen. The screen carries the unread
/// hoist, and `position` is half of `(priority, position, number)` — the
/// order the board itself takes work out of Todo by. Sending what is
/// rendered would let an agent's comment re-rank a column *for good*, the
/// next time anybody dragged anything in it, and the promise the hoist makes
/// is that it never writes.
///
/// `before` is the anchor `resolveDrop` already resolved, and `null` means
/// the end — the same two cases `moveCard` handles, answered here against
/// stored order instead of rendered order.
export function persistedOrder(column: Issue[], moved: number, before: number | null): number[] {
  const stored = column
    .filter((issue) => issue.number !== moved)
    .sort((a, b) => (a.position === b.position ? a.number - b.number : a.position - b.position))
    .map((issue) => issue.number);
  const found = before === null ? -1 : stored.indexOf(before);
  stored.splice(found === -1 ? stored.length : found, 0, moved);
  return stored;
}

/// Write that order back onto the cards as their `position`.
///
/// The server has just been asked to store exactly this, index by index, so
/// without it the client would go on believing the order it replaced — and a
/// second drag in the same column before the board is refetched would send
/// stale slots and undo the first move.
export function withPositions(board: Board, status: IssueStatus, ordered: number[]): Board {
  return {
    ...board,
    [status]: board[status].map((issue) => {
      const at = ordered.indexOf(issue.number);
      return at === -1 ? issue : { ...issue, position: at };
    }),
  };
}

export function placementChanged(before: Board, after: Board, number: number): boolean {
  const beforeStatus = statusOf(before, number);
  const afterStatus = statusOf(after, number);
  if (beforeStatus === null || afterStatus === null) return false;
  if (beforeStatus !== afterStatus) return true;
  const beforeIndex = before[beforeStatus].findIndex((issue) => issue.number === number);
  const afterIndex = after[afterStatus].findIndex((issue) => issue.number === number);
  return beforeIndex !== afterIndex;
}

export function runIndicator(
  activeRuns: IssueRun[],
  number: number,
): 'queued' | 'running' | null {
  const run = activeRuns.find((candidate) => candidate.number === number);
  if (run === undefined) return null;
  return run.status === 'running' ? 'running' : 'queued';
}

/// How long something took. One spelling, because the execution log measures
/// it from a run's own timestamps and the activity feed is handed it already
/// derived — two formatters would give the same run two lengths.
export function formatDuration(ms: number): string {
  const seconds = Math.max(0, Math.round(ms / 1000));
  if (seconds < 60) return `${seconds}s`;
  return `${Math.floor(seconds / 60)}m${String(seconds % 60).padStart(2, '0')}s`;
}

export function runDuration(run: IssueRun, now: number): string | null {
  if (run.started_at_ms == null) return null;
  return formatDuration((run.settled_at_ms ?? now) - run.started_at_ms);
}

/// Tokens counted by the board ceiling: input plus output.
export function tokensOf(run: IssueRun): number {
  return (run.input_tokens ?? 0) + (run.output_tokens ?? 0);
}

// Unknown future states remain unsettled rather than hiding active work.
const SETTLED: ReadonlySet<RunStatus> = new Set<RunStatus>(['done', 'failed', 'cancelled']);

export function unsettledRun<T extends { status: RunStatus }>(runs: readonly T[]): T | null {
  return runs.find((run) => !SETTLED.has(run.status)) ?? null;
}

/// How many runs the execution log shows before the rest fold away. A run is
/// two lines in a 340px rail, so a card that has been retried and commented on
/// all week pushes its own totals off the bottom of the pane.
export const RUN_LOG_HEAD = 5;

export type RunLogView<T> = {
  shown: readonly T[];
  /// Runs the fold is holding back, and how many of those failed. A fold that
  /// silently swallows a failure is the one thing collapsing this list could
  /// break, so the toggle says so on its face.
  hidden: number;
  hiddenFailed: number;
  /// Whether there is a fold to work at all — false on a short log, where the
  /// toggle would be a control that does nothing.
  foldable: boolean;
};

export function runLogView<T extends { status: RunStatus }>(
  runs: readonly T[],
  expanded: boolean,
): RunLogView<T> {
  // The log arrives newest-first, so the head already holds anything still
  // running — and its Cancel button with it. Taken from where the live run
  // actually is rather than assumed: folding away the only control that stops
  // a running card is not a failure worth saving four rows for.
  const live = runs.findIndex((run) => !SETTLED.has(run.status));
  const head = Math.max(RUN_LOG_HEAD, live + 1);
  const rest = runs.slice(head);
  // Folding a single run is a row of history traded for a row of button.
  const foldable = rest.length > 1;
  if (!foldable || expanded) return { shown: runs, hidden: 0, hiddenFailed: 0, foldable };
  return {
    shown: runs.slice(0, head),
    hidden: rest.length,
    hiddenFailed: rest.filter((run) => run.status === 'failed').length,
    foldable,
  };
}

export function dropRejection(issue: Issue, target: IssueStatus): string | null {
  if (target === 'in_progress' && issue.assignee == null) {
    return `Assign an agent first — #${issue.number} is back in ${COLUMN_LABEL[issue.status]}`;
  }
  return null;
}

/// What a completed move should announce, if anything.
///
/// Almost nothing. The board is the control surface and the timeline is the
/// audit trail, so a reorder and an ordinary column change are silent; the
/// one drop worth interrupting for is the one that **started an agent**,
/// because that is the only one that spends money without being asked
/// again. A toast on every move is a toast nobody reads.
export function moveAnnouncement(
  issue: Issue,
  from: IssueStatus,
  handle: string | null,
): string | null {
  const started = issue.status === 'in_progress' && from !== 'in_progress';
  if (!started || issue.assignee == null || handle === null) return null;
  return `Queued for @${handle} — #${issue.number}`;
}

/// Whether the card should show a branch element at all.
///
/// A branch appears only once the work has a commit, which is the same fact
/// as "this card produced something" — so a research card, whose deliverable
/// is its report, never shows one. See the spec's worktree-vs-branch split.
export function hasDeliverable(issue: Issue): boolean {
  return issue.branch != null && issue.branch.length > 0;
}

export function updatedAgo(atMs: number, nowMs: number): string {
  const seconds = Math.max(0, Math.round((nowMs - atMs) / 1000));
  if (seconds < 60) return 'now';
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h`;
  const days = Math.floor(hours / 24);
  if (days < 7) return `${days}d`;
  return `${Math.floor(days / 7)}w`;
}

export function assignableAgents(agents: Agent[]): Agent[] {
  return agents.filter((agent) => agent.framework === 'baybo');
}

export function resolveLanding(
  projects: Project[],
  rememberedId: string | null,
): { kind: 'empty' } | { kind: 'go'; id: string } {
  if (projects.length === 0) return { kind: 'empty' };
  if (rememberedId !== null && projects.some((project) => project.id === rememberedId)) {
    return { kind: 'go', id: rememberedId };
  }
  return { kind: 'go', id: projects[0].id };
}
