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
 * Split a flat listing into columns. Column order within each list follows
 * `position`, which the server keeps dense.
 *
 * Nothing is dropped here. The board state has to hold every card the
 * server has, because a move sends its destination column's *whole* new
 * order — a state built from what is visible would omit exactly the rows
 * nobody is looking at, and the server would renumber around them. Hiding
 * is a render-time concern; see `filterBoard`.
 */
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
/**
 * A resolved placement, named by the card it lands in front of rather than
 * by an index.
 *
 * An index is only meaningful against the list it was resolved on. A drag
 * is resolved against what the operator can *see* — which is a filtered
 * view — and then applied to the whole board, so an index would place the
 * card somewhere else entirely as soon as anything is hidden. `before` is
 * a card number, which means the same thing in both lists; `null` appends.
 */
export type Drop = { status: IssueStatus; before: number | null; issue: Issue };

/**
 * Resolve a drag into a placement.
 *
 * Dropping onto a card means "take that card's slot"; dropping onto a
 * column's body means "append" — which is the only way to reach an empty
 * column, since it has no cards to collide with.
 *
 * `view` is what is rendered, because those are the ids dnd-kit collided
 * against. The result is applied to the full board.
 */
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
    // Already the last card of that column: appending would be a no-op that
    // still costs a request.
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
  // Taking the over-card's slot means landing in front of it — unless the
  // dragged card is already above it in the same column, in which case its
  // slot is the one *after* it. Same rule as the old index arithmetic, said
  // in card numbers so it survives being applied to a longer list.
  const anchorIndex = activeIndex !== -1 && activeIndex < overIndex ? overIndex + 1 : overIndex;
  return { status: overStatus, before: column[anchorIndex]?.number ?? null, issue };
}

/** Apply a placement. Pure, so the optimistic move and its test agree. */
export function moveCard(board: Board, drop: Drop): Board {
  const next = emptyBoard();
  for (const status of COLUMNS) {
    next[status] = board[status].filter((issue) => issue.number !== drop.issue.number);
  }
  const moved: Issue = { ...drop.issue, status: drop.status };
  const column = next[drop.status];
  // An anchor that is not in this column — hidden by a filter, or gone
  // since the drag started — appends rather than throwing the card at
  // index 0, which is where `indexOf`'s -1 would put it.
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
 * The run states that are over. Everything else is unsettled, so a state
 * this bundle has never heard of reads as work still going rather than as
 * nothing at all.
 */
const SETTLED: ReadonlySet<RunStatus> = new Set<RunStatus>(['done', 'failed', 'cancelled']);

/**
 * The run holding this issue's live-run slot, or `null` when it has none.
 *
 * At most one can exist — the store's partial unique index over the
 * unsettled states is what makes that true (`docs/modules/project.md`) —
 * and while it does, nothing can record a second run on the issue. So this
 * one question covers everything the ledger decides for a card.
 *
 * `held` is unsettled without being in flight: nothing is executing, the
 * row is waiting on the board's budget. Callers that care about the
 * difference read the returned run's status; callers that only need "is
 * the slot taken" never have to enumerate the states, which is the point —
 * listing the live ones at a call site is how `held` gets forgotten.
 */
export function unsettledRun<T extends { status: RunStatus }>(runs: readonly T[]): T | null {
  return runs.find((run) => !SETTLED.has(run.status)) ?? null;
}

/**
 * What a held run is waiting for — a note beside a *working* button, not a
 * reason the button is dead.
 *
 * Pressing "run it again" on a held card is the one thing that acts on the
 * ledger's refusal rather than bouncing off it: `retry_run` goes through
 * `enqueue`, which releases what the ceiling allows before it writes, so on
 * a board with room the press starts this very run. Disabling the button
 * here would take away the only control that does anything and replace it
 * with a promise the board keeps only when something else happens on it —
 * holds are released by activity, not by a clock.
 *
 * The wording is the server's own (`HELD_RUN_REFUSAL` in
 * `crates/project/src/manager.rs`), so the note read before the press and
 * the toast that arrives when the ceiling refuses again are one sentence
 * rather than two. It is pinned from the Rust side by
 * `a_retry_on_a_held_run_starts_it_when_the_board_has_room` in
 * `crates/project/tests/manager.rs`.
 */
export const HELD_RUN_NOTE =
  'this run is held — the project is over its daily budget, and starts as soon as there is room';

/**
 * Why running this card refuses to start, or `null` when the button works.
 *
 * A hand-written mirror of `ProjectManager::retry_run` in `crates/project`:
 * its assignee check, and then `runs::accepts_runs`, whose definition of a
 * card the board has finished with is `stages::is_finished` — Done, or
 * cancelled. This sits on the same seam as `commentHint` and `mentionHint`
 * (see `docs/modules/project.md`), for the same reason: the button has to
 * know whether the request would be refused *before* it is sent, so it
 * cannot ask the server. Nothing enforces the correspondence — no generated
 * binding, no shared schema — only the two test suites, one per language,
 * asserting the same cases: `retryRejection` in `boardModel.test.ts` here,
 * and `the_retry_refusals_say_exactly_what_the_button_predicts` in
 * `crates/project/tests/manager.rs` there, which asserts all three sentences
 * verbatim and in this order. A new reason to refuse a run is a change on
 * both sides in the same commit; `cargo test` alone will be green with a
 * button that offers a run the server will reject.
 *
 * The sentences are the server's own, word for word. An operator can still
 * race into the 400 — the card is cancelled in another tab between this
 * render and the click — and the toast that arrives then carries this exact
 * sentence (behind `ProjectError::Invalid`'s `invalid <field>: ` prefix),
 * so the two surfaces are one phrasing of one rule rather than two. The
 * order is the server's too: a cancelled card with nobody on it is refused
 * for the assignee, because that is the answer the click would have brought
 * back.
 *
 * These are the card's own refusals, and only those. What the run ledger
 * does is a separate question, asked of `unsettledRun`: a run in flight
 * hides the button, because its own row sits right below with the stop on
 * it, and a held run leaves it working — pressing it is what releases the
 * hold — with `HELD_RUN_NOTE` beside it. An archived board is refused too,
 * and this page never
 * fetches the project row, so it cannot know: that one still arrives as a
 * toast after the click.
 */
export function retryRejection(issue: {
  status: IssueStatus;
  assignee?: string | null;
  cancelled_at_ms?: number | null;
}): string | null {
  if (issue.assignee == null) return 'an issue with nobody on it cannot be run';
  if (issue.cancelled_at_ms != null) {
    return 'this issue was cancelled — reopen it before running it again';
  }
  if (issue.status === 'done') {
    return 'this issue is done — move it back into the board before running it again';
  }
  return null;
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

/**
 * How long ago a card was touched, in the shortest form that is still
 * unambiguous.
 *
 * Coarse on purpose: a card face has room for four characters, and the
 * question it answers is "is this stale", not "when exactly". Anything
 * within the minute reads as `now` rather than `0m`, which looks like a
 * missing value.
 */
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
