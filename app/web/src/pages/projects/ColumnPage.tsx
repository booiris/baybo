import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Link, Navigate, useNavigate, useParams, useSearchParams } from 'react-router-dom';
import {
  DndContext,
  DragOverlay,
  KeyboardSensor,
  PointerSensor,
  closestCenter,
  useSensor,
  useSensors,
  type DragEndEvent,
  type DragStartEvent,
} from '@dnd-kit/core';
import {
  SortableContext,
  arrayMove,
  sortableKeyboardCoordinates,
  useSortable,
  verticalListSortingStrategy,
} from '@dnd-kit/sortable';
import { CSS } from '@dnd-kit/utilities';
import { RiAddLine, RiArchiveLine, RiArrowLeftLine, RiLoader4Line } from 'react-icons/ri';

import { useAdminClient, useAuth } from '../../api/auth';
import {
  fetchActiveRuns,
  fetchIssues,
  fetchProject,
  fetchTeam,
  moveIssue,
  patchIssue,
  removeAgent,
} from './api';
import {
  COLUMNS,
  COLUMN_LABEL,
  COLUMN_PILL_LABEL,
  HEADER_ACTION,
  HEADER_ACTION_DEAD,
  HEADER_ACTION_OFF,
  HEADER_ACTION_ON,
  STATUS_PILL,
  type Agent,
  type Board,
  type Issue,
  type IssueRun,
  type IssueStatus,
  type Project,
  assignableAgents,
  cardDragId,
  columnHasNews,
  dropRejection,
  emptyBoard,
  findIssue,
  groupByStatus,
  hasDeliverable,
  readingOrder,
  liveCount,
  moveAnnouncement,
  moveCard,
  parseDragId,
  persistedOrder,
  placementChanged,
  runIndicator,
  statusOf,
  updatedAgo,
  withPin,
  withPositions,
} from './boardModel';
import { boardFilterParams, filterBoard, parseBoardFilter } from './boardFilter';
import type { BoardFilter } from './boardFilter';
import { BoardFilterMenu } from './BoardFilterMenu';
import { generatedPortrait, useTeamPortraits, type Portrait } from './portrait';
import { writeLastProjectId } from './lastProject';
import { CreateIssueModal } from './CreateIssueModal';
import { AgentProfile } from './AgentProfile';
import { FloatingPanel } from './FloatingPanel';
import { Picker } from './Picker';
import { statusChipShape, statusOptions } from './issueFields';
import {
  AssigneeFace,
  BlockedBadge,
  BranchChip,
  FailedBadge,
  PinButton,
  PRIORITY_MARK,
  SubIssueRing,
  UnassignedMark,
  UnreadPill,
} from './cardChrome';
import { handleOf } from './teamModel';
import { invalidateAttention } from './useAttention';
import { ToastStack, useToasts } from './Toasts';
import { useBoardStream } from './useBoardStream';

/// One column of the board, as a whole page.
///
/// The board's columns are honest about what they are — five narrow queues —
/// and a queue holding thirty cards reads as a smear of two-line tiles. This
/// page is the same column at reading width: one row per card, the card's
/// whole vocabulary (priority mark, badges, branch, faces, unread) laid out
/// in aligned cells, in the same order the board renders and writes.
///
/// It stays the board in every rule that matters: the unread hoist is the
/// reading order and is never written, a drag sends `persistedOrder`, a
/// refetch is held while a card is in the air, and the filter is the same
/// URL vocabulary — so the narrowing survives the zoom in and back out.
export function ColumnPage() {
  const { pid, status: statusParam } = useParams<{ pid: string; status: string }>();
  const projectId = pid ?? '';
  const status: IssueStatus | null = (COLUMNS as readonly string[]).includes(statusParam ?? '')
    ? (statusParam as IssueStatus)
    : null;
  const client = useAdminClient();
  const { logout } = useAuth();
  const navigate = useNavigate();
  const { toasts, pushToast, dismissToast } = useToasts();

  const [project, setProject] = useState<Project | null>(null);
  const [team, setTeam] = useState<Agent[]>([]);
  const portrait = useTeamPortraits(team);
  const [activeRuns, setActiveRuns] = useState<IssueRun[]>([]);
  const [board, setBoard] = useState<Board>(emptyBoard);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);
  const [dragging, setDragging] = useState<Issue | null>(null);
  const [createOpen, setCreateOpen] = useState(false);
  const [profileOf, setProfileOf] = useState<string | null>(null);
  const [params, setParams] = useSearchParams();
  const filter = useMemo(() => parseBoardFilter(params), [params]);

  // Non-null for exactly as long as a row is in the air — the board the drag
  // started from, and the flag that holds a refetch back until it lands.
  const preDrag = useRef<Board | null>(null);
  const missedRefresh = useRef(false);

  useEffect(() => {
    let canceled = false;
    async function load() {
      setLoading(true);
      const [projectOutcome, issuesOutcome, agentsOutcome, runsOutcome] = await Promise.all([
        fetchProject(client, projectId),
        fetchIssues(client, projectId),
        fetchTeam(client, projectId),
        fetchActiveRuns(client, projectId),
      ]);
      if (canceled) return;
      if (projectOutcome.kind === 'unauthorized' || issuesOutcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (projectOutcome.kind === 'failed') {
        setError(projectOutcome.message);
        setLoading(false);
        return;
      }
      if (issuesOutcome.kind === 'failed') {
        setError(issuesOutcome.message);
        setLoading(false);
        return;
      }
      setError(null);
      setProject(projectOutcome.value);
      // Hoisted here, to the board itself, for the board page's reason: one
      // order on screen, in the drag, and under `resolveDrop` — never a
      // second one that only exists while nothing is being dragged.
      setBoard(readingOrder(groupByStatus(issuesOutcome.value)));
      if (agentsOutcome.kind === 'ok') setTeam(agentsOutcome.value);
      setActiveRuns(runsOutcome.kind === 'ok' ? runsOutcome.value : []);
      setLoading(false);
      // A full project surface — same fetches, same mutations — so being
      // here counts as visiting the project the way the board does, and the
      // rail's Projects entry reopens this project, not the one before it.
      writeLastProjectId(projectId);
    }
    void load();
    return () => {
      canceled = true;
    };
  }, [client, logout, projectId, refreshKey]);

  const refetch = useCallback(() => {
    setRefreshKey((key) => key + 1);
    invalidateAttention(client);
  }, [client]);

  useBoardStream(
    projectId,
    null,
    useCallback(() => {
      // Held while a row is in the air: a refetch re-keys every row and
      // re-ranks the very list being dragged in, so the drop would resolve
      // against a layout the operator never aimed at.
      if (preDrag.current !== null) {
        missedRefresh.current = true;
        return;
      }
      refetch();
    }, [refetch]),
  );

  // The frame the drag held back, answered the moment it lands.
  useEffect(() => {
    if (dragging !== null || !missedRefresh.current) return;
    missedRefresh.current = false;
    refetch();
  }, [dragging, refetch]);

  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 4 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );

  const view = useMemo(() => filterBoard(board, filter, team), [board, filter, team]);
  const list = useMemo(() => (status === null ? [] : view[status]), [status, view]);
  // One array per set of rows: `SortableContext` keys its context value off
  // this and compares it by identity, so a fresh one per render re-renders
  // every row and disables the sort animation.
  const items = useMemo(() => list.map((issue) => cardDragId(issue.number)), [list]);

  type Commit = (before: Board, next: Board, number: number, anchor: number | null) => Promise<void>;
  // The failed-move toast's Retry re-enters the commit with the same
  // arguments; a ref reaches the current one without the callback having to
  // depend on itself.
  const commitRef = useRef<Commit | null>(null);
  const commitMove: Commit = useCallback(
    async (before, next, number, anchor) => {
      setBoard(next);
      const moved = findIssue(next, number);
      if (moved === null) return;
      // The refusal names the column the card snapped back to, so it reads
      // the pre-move card — the moved copy already wears the very column it
      // is being refused.
      const refusal = dropRejection(findIssue(before, number) ?? moved, moved.status);
      if (refusal !== null) {
        setBoard(before);
        pushToast('warn', refusal);
        return;
      }
      const from = statusOf(before, number);
      // What the column stores, not what it shows: the hoist is a reading
      // order and a move must not write it.
      const ordered = persistedOrder(next[moved.status], number, anchor);
      setBoard(withPositions(next, moved.status, ordered));
      const outcome = await moveIssue(client, projectId, number, moved.status, ordered);
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setBoard(before);
        pushToast('err', `Move failed, rolled back — ${outcome.message}`, {
          label: 'Retry',
          run: () => {
            void commitRef.current?.(before, next, number, anchor);
          },
        });
        return;
      }
      const said = moveAnnouncement(
        moved,
        from ?? moved.status,
        moved.assignee == null ? null : handleOf(team, moved.assignee),
      );
      if (said !== null) pushToast('ok', said);
    },
    [client, logout, projectId, pushToast, team],
  );
  commitRef.current = commitMove;

  const onDragStart = useCallback(
    (event: DragStartEvent) => {
      const target = parseDragId(String(event.active.id));
      const card = target?.kind === 'card' ? findIssue(board, target.number) : null;
      preDrag.current = card === null ? null : board;
      setDragging(card);
    },
    [board],
  );

  const onDragEnd = useCallback(
    async (event: DragEndEvent) => {
      setDragging(null);
      const before = preDrag.current;
      preDrag.current = null;
      if (before === null || status === null) return;
      const target = parseDragId(String(event.active.id));
      if (target?.kind !== 'card') return;
      const over = event.over === null ? null : parseDragId(String(event.over.id));
      if (over?.kind !== 'card' || over.number === target.number) return;
      const column = filterBoard(board, filter, team)[status];
      const from = column.findIndex((row) => row.number === target.number);
      const at = column.findIndex((row) => row.number === over.number);
      if (from === -1 || at === -1) return;
      // The order dnd-kit previewed, read back in the one shape
      // `persistedOrder` accepts: the card this one now sits in front of.
      const anchor = arrayMove(column, from, at)[at + 1]?.number ?? null;
      const issue = findIssue(board, target.number);
      if (issue === null) return;
      const next = moveCard(board, { status, before: anchor, issue });
      if (!placementChanged(board, next, target.number)) return;
      await commitMove(board, next, target.number, anchor);
    },
    [board, commitMove, filter, status, team],
  );

  const onDragCancel = useCallback(() => {
    setDragging(null);
    preDrag.current = null;
  }, []);

  const relocate = useCallback(
    (issue: Issue, to: IssueStatus) => {
      if (to === issue.status) return;
      // No anchor: a card sent to another column joins the end of its queue,
      // exactly as a drop on the column's body does on the board.
      void commitMove(board, moveCard(board, { status: to, before: null, issue }), issue.number, null);
    },
    [board, commitMove],
  );

  /// The board's own pin, on the same terms: optimistic, rolled back and
  /// named on failure. See `ProjectBoardPage.togglePin`.
  const togglePin = useCallback(
    async (number: number, pinned: boolean) => {
      const before = board;
      setBoard(withPin(board, number, pinned));
      const outcome = await patchIssue(client, projectId, number, { pinned });
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setBoard(before);
        pushToast('err', `${pinned ? 'Pin' : 'Unpin'} failed — ${outcome.message}`);
      }
    },
    [board, client, logout, projectId, pushToast],
  );

  const openIssue = useCallback(
    (number: number) => {
      navigate(`/projects/${encodeURIComponent(projectId)}/issues/${number}`);
    },
    [navigate, projectId],
  );

  /// One door to the profile, from every face on the page.
  const openAgent = useCallback((agentId: string) => {
    setProfileOf(agentId);
  }, []);

  const archived = useMemo(() => project?.archived_at_ms != null, [project]);

  if (status === null) {
    return <Navigate to={`/projects/${encodeURIComponent(projectId)}`} replace />;
  }

  if (loading && project === null) {
    return (
      <div className="flex flex-1 items-center justify-center">
        <RiLoader4Line className="text-3xl text-ink-soft animate-spin" />
      </div>
    );
  }

  const total = liveCount(board[status]);

  return (
    <div className="flex flex-col h-full min-h-0">
      <h1 className="sr-only">{`${COLUMN_LABEL[status]} — ${project?.name ?? ''}`}</h1>
      <header className="h-12 shrink-0 px-4 border-b-2 border-black flex items-center gap-3 bg-canvas">
        <Link
          to={{ pathname: `/projects/${encodeURIComponent(projectId)}`, search: params.toString() }}
          className="inline-flex shrink-0 items-center gap-1 font-mono text-[0.72rem] text-ink-soft hover:text-ink"
        >
          <RiArrowLeftLine /> Board
        </Link>
        {archived ? (
          <span className="inline-flex shrink-0 items-center gap-1 border-2 border-warn bg-warn/10 text-warn rounded-md px-2 py-0.5 font-mono text-[0.62rem] font-bold uppercase tracking-wider">
            <RiArchiveLine /> Archived — read only
          </span>
        ) : null}
        {/* The five stages as a row of presses, so the zoomed-in view keeps
            the board's whole width one press away instead of a round trip
            through the board. */}
        <nav aria-label="Stages" className="flex min-w-0 items-center gap-1.5 overflow-x-auto">
          {COLUMNS.map((tab) => {
            const live = liveCount(view[tab]);
            const whole = liveCount(board[tab]);
            return (
              <Link
                key={tab}
                to={{
                  pathname: `/projects/${encodeURIComponent(projectId)}/board/${tab}`,
                  search: params.toString(),
                }}
                aria-current={tab === status ? 'page' : undefined}
                className={`${HEADER_ACTION} shrink-0 ${tab === status ? HEADER_ACTION_ON : HEADER_ACTION_OFF}`}
              >
                {COLUMN_PILL_LABEL[tab]}{' '}
                <span
                  className="rounded-full bg-ink text-brand font-mono text-[0.56rem] px-1.5 leading-[1.05rem] tabular-nums"
                  title={
                    live === whole
                      ? `${whole} live`
                      : `${live} of ${whole} live cards match the filter`
                  }
                >
                  {live === whole ? whole : `${live}/${whole}`}
                </span>
                {/* A dot, not a number: pressing a tab cannot discharge what
                    it shows — opening cards does. The stage on screen wears
                    none, because its rows carry the counts themselves. */}
                {tab !== status && columnHasNews(board[tab]) ? (
                  <span
                    title={`Something new in ${COLUMN_LABEL[tab]}`}
                    className="w-2 h-2 shrink-0 rounded-full border border-black bg-err"
                  />
                ) : null}
              </Link>
            );
          })}
        </nav>
        <div className="ml-auto flex items-center gap-2">
          <BoardFilterMenu
            filter={filter}
            team={team}
            onChange={(next: BoardFilter) => {
              setParams(boardFilterParams(next), { replace: true });
            }}
          />
          <button
            type="button"
            disabled={archived}
            title={`New issue in ${COLUMN_LABEL[status]}`}
            onClick={() => {
              setCreateOpen(true);
            }}
            className={`${HEADER_ACTION} ${archived ? HEADER_ACTION_DEAD : HEADER_ACTION_OFF}`}
          >
            <RiAddLine aria-hidden />
            New issue
          </button>
        </div>
      </header>

      {error != null ? (
        <div className="m-4 bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm break-words">
          {error}
        </div>
      ) : null}

      {/* Positioning context for the profile panel: an absolute box inside
          the scroller would be placed against the content and scroll away
          with it. */}
      <div className="relative flex-1 min-h-0 flex flex-col">
        <div className="flex-1 min-h-0 overflow-y-auto bg-surface">
          <div className="mx-auto w-full max-w-6xl p-4">
          <DndContext
            sensors={sensors}
            collisionDetection={closestCenter}
            onDragStart={onDragStart}
            onDragEnd={(event) => {
              void onDragEnd(event);
            }}
            onDragCancel={onDragCancel}
          >
            {list.length > 0 ? (
              <div className="border-2 border-black rounded-md bg-canvas shadow-brutal-sm divide-y divide-black/15 overflow-hidden">
                <SortableContext items={items} strategy={verticalListSortingStrategy}>
                  {list.map((issue) => (
                    <ColumnRow
                      key={issue.number}
                      issue={issue}
                      run={runIndicator(activeRuns, issue.number)}
                      team={team}
                      portrait={portrait}
                      readOnly={archived}
                      onOpen={openIssue}
                      onOpenAgent={openAgent}
                      onRelocate={relocate}
                      onTogglePin={togglePin}
                    />
                  ))}
                </SortableContext>
              </div>
            ) : (
              <p className="border-2 border-dashed border-black/30 rounded-md p-10 text-center font-mono text-[0.68rem] text-ink-soft leading-relaxed">
                {total === 0 && board[status].length === 0 ? (
                  <>
                    No issues in {COLUMN_LABEL[status]}
                    <br />
                    Press “New issue”, or drag one in on the board
                  </>
                ) : (
                  'No matches'
                )}
              </p>
            )}

            <DragOverlay>
              {dragging !== null ? (
                <RowBody
                  issue={dragging}
                  run={runIndicator(activeRuns, dragging.number)}
                  team={team}
                  overlay
                />
              ) : null}
            </DragOverlay>
          </DndContext>
          </div>
        </div>
        {profileOf !== null
          ? (() => {
              const agent = team.find((row) => row.id === profileOf);
              return agent === undefined ? null : (
                <FloatingPanel
                  // A fresh panel per agent: pressing another face while one
                  // is open must switch to it, not have the outgoing panel's
                  // pending unmount close its replacement.
                  key={agent.id}
                  onDismiss={() => {
                    setProfileOf(null);
                  }}
                >
                  {(leave) => (
                    <AgentProfile
                      agent={agent}
                      team={team}
                      issues={Object.values(board).flat()}
                      activeRuns={activeRuns}
                      readOnly={archived}
                      projectId={projectId}
                      onChanged={refetch}
                      onClose={leave}
                      onRemove={(row) => {
                        setProfileOf(null);
                        void removeAgent(client, projectId, row.id).then((outcome) => {
                          if (outcome.kind === 'unauthorized') {
                            logout();
                            return;
                          }
                          pushToast(
                            outcome.kind === 'ok' ? 'ok' : 'err',
                            outcome.kind === 'ok'
                              ? `@${row.handle} left the project`
                              : outcome.message,
                          );
                          refetch();
                        });
                      }}
                    />
                  )}
                </FloatingPanel>
              );
            })()
          : null}
      </div>

      {createOpen ? (
        <CreateIssueModal
          projectId={projectId}
          status={status}
          agents={assignableAgents(team)}
          portrait={portrait}
          parents={Object.values(board)
            .flat()
            .filter((row) => row.parent == null && row.cancelled_at_ms == null)
            .map((row) => ({ number: row.number, title: row.title }))}
          onClose={() => {
            setCreateOpen(false);
          }}
          onCreated={() => {
            setCreateOpen(false);
            refetch();
          }}
        />
      ) : null}

      <ToastStack toasts={toasts} onDismiss={dismissToast} />
    </div>
  );
}

function ColumnRow({
  issue,
  run,
  team,
  portrait,
  readOnly,
  onOpen,
  onOpenAgent,
  onRelocate,
  onTogglePin,
}: {
  issue: Issue;
  run: 'queued' | 'running' | null;
  team: Agent[];
  portrait: Portrait;
  readOnly: boolean;
  onOpen: (number: number) => void;
  onOpenAgent: (agentId: string) => void;
  onRelocate: (issue: Issue, to: IssueStatus) => void;
  onTogglePin: (number: number, pinned: boolean) => void;
}) {
  const { attributes, listeners, setNodeRef, setActivatorNodeRef, transform, transition, isDragging } =
    useSortable({
      id: cardDragId(issue.number),
      disabled: readOnly,
    });

  return (
    <div
      // The wrapper is also the keyboard sensor's activator node, so its
      // `event.target !== activator` guard engages: Enter on a control
      // inside the row — the status picker, the pin, the face — presses
      // that control instead of silently lifting the row.
      ref={(node) => {
        setNodeRef(node);
        setActivatorNodeRef(node);
      }}
      style={{ transform: CSS.Transform.toString(transform), transition }}
      className={`touch-none ${isDragging ? 'opacity-40' : ''}`}
      {...attributes}
      {...listeners}
      onClick={() => {
        onOpen(issue.number);
      }}
    >
      <RowBody
        issue={issue}
        run={run}
        team={team}
        portrait={portrait}
        readOnly={readOnly}
        onOpenAgent={onOpenAgent}
        onRelocate={onRelocate}
        onTogglePin={onTogglePin}
      />
    </div>
  );
}

/// One card as one row: the same vocabulary the board's tile wears, laid in
/// aligned cells so thirty of them read as a table instead of a smear.
function RowBody({
  issue,
  run = null,
  team = [],
  portrait = generatedPortrait,
  readOnly = false,
  overlay = false,
  onOpenAgent,
  onRelocate,
  onTogglePin,
}: {
  issue: Issue;
  run?: 'queued' | 'running' | null;
  team?: Agent[];
  portrait?: Portrait;
  readOnly?: boolean;
  /// The drag overlay is a picture of a row, not a row: no picker, its own
  /// frame and shadow.
  overlay?: boolean;
  /// Absent on the drag overlay, where the face is a picture and not a door.
  onOpenAgent?: (agentId: string) => void;
  onRelocate?: (issue: Issue, to: IssueStatus) => void;
  /// Absent on the drag overlay, like `onRelocate`.
  onTogglePin?: (number: number, pinned: boolean) => void;
}) {
  const cancelled = issue.cancelled_at_ms != null;
  const priority = PRIORITY_MARK[issue.priority];
  return (
    <div
      className={`group flex items-center gap-2.5 px-3 py-2 font-mono ${
        overlay
          ? 'bg-surface border-2 border-black rounded-md shadow-brutal'
          : 'cursor-pointer hover:bg-brand/10'
      } ${cancelled ? 'opacity-55' : ''}`}
    >
      <span className="w-4 shrink-0 flex justify-center">
        {onTogglePin === undefined ? null : (
          <PinButton
            pinned={issue.pinned}
            disabled={readOnly}
            onToggle={(pinned) => {
              onTogglePin(issue.number, pinned);
            }}
          />
        )}
      </span>
      <span
        className={`w-6 shrink-0 text-center text-[0.62rem] font-bold ${
          priority === null ? '' : priority.tone
        }`}
        title={priority === null ? undefined : `Priority: ${issue.priority}`}
      >
        {priority === null ? '' : priority.glyph}
      </span>
      <span className="w-11 shrink-0 text-[0.66rem] font-bold text-ink-soft tabular-nums">
        #{issue.number}
      </span>
      <span
        className={`min-w-0 flex-1 truncate text-[0.78rem] font-bold ${
          cancelled ? 'line-through' : ''
        }`}
        title={issue.title}
      >
        {issue.title}
      </span>
      {issue.blocked_reason != null ? (
        <span className="flex shrink-0 items-center">
          <BlockedBadge reason={issue.blocked_reason} />
        </span>
      ) : null}
      {issue.last_run_failed ? (
        <span className="flex shrink-0 items-center">
          <FailedBadge />
        </span>
      ) : null}
      {hasDeliverable(issue) && issue.branch != null ? (
        <span className="hidden xl:flex shrink-0 items-center max-w-44">
          <BranchChip branch={issue.branch} />
        </span>
      ) : null}
      <span className="hidden sm:flex w-12 shrink-0 justify-end">
        {issue.sub_issues != null ? <SubIssueRing progress={issue.sub_issues} /> : null}
      </span>
      <span
        className={`hidden md:block w-14 shrink-0 text-right text-[0.54rem] font-bold uppercase ${
          run === 'running' ? 'text-ok' : 'text-ink-soft'
        }`}
      >
        {run === 'running' ? 'working' : run === 'queued' ? 'queued' : ''}
      </span>
      {/* The picker's own presses must not fall through to the row link
          under them — the row opens the card, and this cell moves it. */}
      <span
        className="w-[5.6rem] shrink-0 flex justify-center"
        onClick={(event) => {
          event.preventDefault();
          event.stopPropagation();
        }}
      >
        {overlay || readOnly || onRelocate === undefined ? (
          <span className={`${statusChipShape} px-2 ${STATUS_PILL[issue.status]}`}>
            {COLUMN_PILL_LABEL[issue.status]}
          </span>
        ) : (
          <Picker
            label={`Status of #${issue.number}`}
            title="Move this card to another column"
            value={issue.status}
            disabled={false}
            options={statusOptions}
            onPick={(picked) => {
              onRelocate(issue, picked as IssueStatus);
            }}
            triggerClassName={`${statusChipShape} pl-2 pr-0.5 ${STATUS_PILL[issue.status]} hover:border-black`}
          >
            {COLUMN_PILL_LABEL[issue.status]}
          </Picker>
        )}
      </span>
      {/* Hidden below `sm` with the time cell: the always-on set has to fit
          a phone, and these two are the widest facts the title can spare —
          the card itself still names its assignee one tap away. */}
      <span className="hidden sm:flex w-28 shrink-0 items-center gap-1.5 min-w-0">
        {issue.assignee != null ? (
          <AssigneeFace
            handle={handleOf(team, issue.assignee)}
            src={portrait(issue.assignee)}
            run={run}
            onOpen={
              onOpenAgent === undefined
                ? undefined
                : () => {
                    onOpenAgent(issue.assignee ?? '');
                  }
            }
          />
        ) : (
          <UnassignedMark />
        )}
      </span>
      <span
        className="hidden sm:block w-9 shrink-0 text-right text-[0.6rem] text-ink-soft tabular-nums"
        title={`Last touched ${new Date(issue.updated_at_ms).toLocaleString()}`}
      >
        {updatedAgo(issue.updated_at_ms, Date.now())}
      </span>
      <span className="w-7 shrink-0 flex justify-end">
        {issue.unread > 0 ? <UnreadPill unread={issue.unread} /> : null}
      </span>
    </div>
  );
}
