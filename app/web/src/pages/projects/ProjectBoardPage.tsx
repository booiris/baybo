import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useNavigate, useParams, useSearchParams } from 'react-router-dom';
import {
  DndContext,
  DragOverlay,
  KeyboardSensor,
  PointerSensor,
  useDroppable,
  useSensor,
  useSensors,
  type DragEndEvent,
  type DragOverEvent,
  type DragStartEvent,
} from '@dnd-kit/core';
import {
  SortableContext,
  sortableKeyboardCoordinates,
  useSortable,
  verticalListSortingStrategy,
} from '@dnd-kit/sortable';
import { CSS } from '@dnd-kit/utilities';
import {
  RiAddLine,
  RiArchiveLine,
  RiCheckDoubleLine,
  RiFullscreenLine,
  RiLoader4Line,
  RiSettings3Line,
} from 'react-icons/ri';

import { useAdminClient, useAuth } from '../../api/auth';
import { IconButton } from '../../components/IconButton';
import {
  fetchActiveRuns,
  fetchTeam,
  hireAgent,
  removeAgent,
  fetchIssues,
  fetchProject,
  fetchProjects,
  markProjectRead,
  moveIssue,
  patchIssue,
} from './api';
import {
  COLUMNS,
  COLUMN_LABEL,
  type Agent,
  type Board,
  type Issue,
  type IssueRun,
  type IssueStatus,
  type Project,
  anchorOf,
  cardDragId,
  assignableAgents,
  columnDropId,
  dropRejection,
  emptyBoard,
  findIssue,
  groupByStatus,
  hasDeliverable,
  HEADER_ACTION,
  HEADER_ACTION_DEAD,
  HEADER_ACTION_OFF,
  HEADER_ACTION_ON,
  readingOrder,
  liveCount,
  moveAnnouncement,
  statusOf,
  unreadTotal,
  updatedAgo,
  moveCard,
  parseDragId,
  persistedOrder,
  placementChanged,
  resolveDrop,
  runIndicator,
  withPin,
  withPositions,
} from './boardModel';
import { boardCollisionDetection } from './dropTarget';
import {
  AssigneeFace,
  BlockedBadge,
  BranchChip,
  FailedBadge,
  PinButton,
  PRIORITY_MARK,
  RunWord,
  SubIssueRing,
  UnassignedMark,
  UnreadPill,
} from './cardChrome';
import type { AvatarRun } from './Avatar';
import { FloatingPanel } from './FloatingPanel';
import { generatedPortrait, useTeamPortraits, type Portrait } from './portrait';
import { writeLastProjectId } from './lastProject';
import { CreateIssueModal } from './CreateIssueModal';
import { ProjectSwitcher } from './ProjectSwitcher';
import { ActivityDrawer } from './ActivityDrawer';
import { AgentProfile } from './AgentProfile';
import { ProjectSettings } from './ProjectSettings';
import { BoardFilterMenu } from './BoardFilterMenu';
import { OverCeilingChip } from './OverCeilingChip';
import { boardFilterParams, filterBoard, parseBoardFilter } from './boardFilter';
import type { BoardFilter } from './boardFilter';
import { invalidateAttention } from './useAttention';
import { TeamStrip } from './TeamStrip';
import { handleOf } from './teamModel';
import { ToastStack, useToasts } from './Toasts';
import { useBoardStream } from './useBoardStream';
import { useBoardActivity } from './useBoardActivity';

export function ProjectBoardPage() {
  const { pid } = useParams<{ pid: string }>();
  const projectId = pid ?? '';
  const client = useAdminClient();
  const { logout } = useAuth();
  const navigate = useNavigate();
  const { toasts, pushToast, dismissToast } = useToasts();

  const [project, setProject] = useState<Project | null>(null);
  const [projects, setProjects] = useState<Project[]>([]);
  const [team, setTeam] = useState<Agent[]>([]);
  const portrait = useTeamPortraits(team);
  const [activeRuns, setActiveRuns] = useState<IssueRun[]>([]);
  const [board, setBoard] = useState<Board>(emptyBoard);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);
  const [dragging, setDragging] = useState<Issue | null>(null);
  const [dropInto, setDropInto] = useState<IssueStatus | null>(null);
  const [createIn, setCreateIn] = useState<IssueStatus | null>(null);
  const [params, setParams] = useSearchParams();
  const filter = useMemo(() => parseBoardFilter(params), [params]);
  const [showActivity, setShowActivity] = useState(false);
  const [profileOf, setProfileOf] = useState<string | null>(null);
  const [showSettings, setShowSettings] = useState(false);
  const [hireOpen, setHireOpen] = useState(false);

  // onDragOver mutates the preview, so rollback needs the drag-start board.
  // Non-null for exactly as long as a card is in the air, which is also what
  // `dragging` says — the two are set and cleared together.
  const preDrag = useRef<Board | null>(null);
  // A `project_changed` frame that arrived while a card was in the air, and
  // has still to be answered.
  const missedRefresh = useRef(false);

  useEffect(() => {
    let canceled = false;
    async function load() {
      setLoading(true);
      const [projectOutcome, issuesOutcome, listOutcome, agentsOutcome, runsOutcome] =
        await Promise.all([
          fetchProject(client, projectId),
          fetchIssues(client, projectId),
          fetchProjects(client, true),
          fetchTeam(client, projectId),
          fetchActiveRuns(client, projectId),
        ]);
      if (canceled) return;
      if (
        projectOutcome.kind === 'unauthorized' ||
        issuesOutcome.kind === 'unauthorized' ||
        listOutcome.kind === 'unauthorized'
      ) {
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
      // The hoist is applied **here**, to the board itself, and not to the
      // rendered view — so the order on screen is the order every drag is
      // resolved in and the order a move writes. Held one layer up it was a
      // second order that only existed while nothing was being dragged, and
      // the switch between the two happened in the same commit as dnd-kit's
      // drag start: a card jumped slots inside its own column before the
      // first `over` resolved, and a 4px twitch on a card that had never
      // moved posted a reorder.
      setBoard(readingOrder(groupByStatus(issuesOutcome.value)));
      if (listOutcome.kind === 'ok') setProjects(listOutcome.value);
      if (agentsOutcome.kind === 'ok') setTeam(agentsOutcome.value);
      setActiveRuns(runsOutcome.kind === 'ok' ? runsOutcome.value : []);
      setLoading(false);
      writeLastProjectId(projectId);
      // Nothing is marked read here. Opening a board is not reading the
      // question asked on one of its cards, and a stamp at this point also
      // covered everything written between the fetches above and the POST
      // landing — swallowing items that were never rendered.
    }
    void load();
    return () => {
      canceled = true;
    };
  }, [client, logout, projectId, refreshKey]);

  // Move requests use the full column order, never this filtered view.
  const view = useMemo(() => filterBoard(board, filter, team, activeRuns), [activeRuns, board, filter, team]);

  const unread = useMemo(() => unreadTotal(board), [board]);
  // The ceiling's own numbers, for the notice below the header. Refetched on
  // the same signal the board is, so a released hold takes the notice with
  // it rather than leaving it up until something else happens.
  const activity = useBoardActivity(true, refreshKey);
  const heldCount = useMemo(
    () => activeRuns.filter((run) => run.status === 'held').length,
    [activeRuns],
  );

  const refetch = useCallback(() => {
    setRefreshKey((key) => key + 1);
    // The board's counts and the rail's dot are the same facts read
    // twice. Without this the dot trailed the board it describes by up
    // to a poll interval, which is how a signal the operator had just
    // discharged read as one that would not clear.
    invalidateAttention(client);
  }, [client]);

  useBoardStream(
    projectId,
    null,
    useCallback(() => {
      // Not under a card that is in the air. A refetch replaces every column
      // wholesale — the cards re-key, and the unread hoist re-ranks the very
      // column being dragged in — so the drop would resolve against a layout
      // the operator never aimed at, and the rollback in `onDragEnd` would
      // then write back a board from before the frame landed.
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

  const onDragStart = useCallback(
    (event: DragStartEvent) => {
      const target = parseDragId(String(event.active.id));
      const card = target?.kind === 'card' ? findIssue(board, target.number) : null;
      preDrag.current = card === null ? null : board;
      setDragging(card);
    },
    [board],
  );

  // dnd-kit needs cross-column previews here to open the destination gap.
  //
  // Resolved against `board` from the closure rather than inside a `setBoard`
  // updater: dnd-kit dispatches this from an effect keyed on the target, and
  // reads the handler out of a ref it refreshes every commit, so the board
  // here is the one that was just rendered. The updater form also ran
  // `setDropInto` from inside a state updater, which React is free to call
  // during render and more than once.
  const onDragOver = useCallback(
    (event: DragOverEvent) => {
      const overId = event.over === null ? null : String(event.over.id);
      const drop = resolveDrop(filterBoard(board, filter, team, activeRuns), String(event.active.id), overId);
      // A cursor over nothing keeps the preview — and the destination outline
      // — where they were; only aiming somewhere new moves them.
      if (drop === null) return;
      setDropInto(drop.status);
      const next = moveCard(board, drop);
      // `moveCard` rebuilds the board unconditionally, so a resolution that
      // reproduces the current order still costs a commit — and every commit
      // during a drag re-measures every droppable.
      if (placementChanged(board, next, drop.issue.number)) setBoard(next);
    },
    [activeRuns, board, filter, team],
  );

  const onDragEnd = useCallback(
    async (event: DragEndEvent) => {
      setDragging(null);
      setDropInto(null);
      const before = preDrag.current;
      preDrag.current = null;
      if (before === null) return;

      const target = parseDragId(String(event.active.id));
      if (target?.kind !== 'card') return;
      const number = target.number;

      const drop = resolveDrop(
        filterBoard(board, filter, team, activeRuns),
        String(event.active.id),
        event.over === null ? null : String(event.over.id),
      );
      // Released over nothing — the 12px between two columns, or off the board
      // — keeps the preview instead of rolling it back. The preview is the
      // promise the whole drag makes, and a card that visibly sits in Review
      // for the length of a drag and then snaps home on release reads as the
      // board having eaten the move. Escape still cancels.
      const next = drop === null ? board : moveCard(board, drop);
      if (!placementChanged(before, next, number)) {
        setBoard(before);
        return;
      }
      setBoard(next);

      const issue = findIssue(next, number);
      if (issue === null) return;
      // The refusal names the column the card snapped back to, so it must
      // read the pre-move card — the moved copy already wears the very
      // column it is being refused.
      const refusal = dropRejection(findIssue(before, number) ?? issue, issue.status);
      if (refusal !== null) {
        setBoard(before);
        pushToast('warn', refusal);
        return;
      }
      const from = statusOf(before, number);
      // What the column stores, not what it shows: the hoist is a reading
      // order and a move must not write it. `withPositions` keeps the client
      // believing what it just asked the server to store, so a second drag
      // before the refetch does not send slots the first one replaced.
      const ordered = persistedOrder(
        next[issue.status],
        number,
        drop === null ? anchorOf(next[issue.status], number) : drop.before,
      );
      setBoard(withPositions(next, issue.status, ordered));
      const outcome = await moveIssue(client, projectId, number, issue.status, ordered);
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setBoard(before);
        pushToast('err', `Move failed, rolled back — ${outcome.message}`, {
          label: 'Retry',
          run: () => {
            // The drag-start board comes out of a ref this handler has already
            // cleared, so a retry that does not hand it back falls straight
            // out of the guard above and the button does nothing at all.
            preDrag.current = before;
            void onDragEnd(event);
          },
        });
        return;
      }
      // Silent unless the drop started an agent: see `moveAnnouncement`.
      const said = moveAnnouncement(
        issue,
        from ?? issue.status,
        issue.assignee == null ? null : handleOf(team, issue.assignee),
      );
      if (said !== null) pushToast('ok', said);
    },
    [activeRuns, board, client, filter, logout, projectId, pushToast, team],
  );

  const onDragCancel = useCallback(() => {
    setDragging(null);
    setDropInto(null);
    const before = preDrag.current;
    preDrag.current = null;
    if (before !== null) setBoard(before);
  }, []);

  const markBoardRead = useCallback(async () => {
    const outcome = await markProjectRead(client, projectId);
    if (outcome.kind === 'unauthorized') {
      logout();
      return;
    }
    if (outcome.kind === 'failed') {
      pushToast('err', `Mark read failed — ${outcome.message}`);
      return;
    }
    // No toast on success: every badge on the board goes out at once, which
    // says it better than a line of text does.
    setRefreshKey((key) => key + 1);
    invalidateAttention(client);
  }, [client, logout, projectId, pushToast]);

  /// The card moves on the press and the write follows it.
  ///
  /// Optimistic rather than a refetch, because the whole point of the pin is
  /// where the card lands and a round trip's worth of stillness after
  /// pressing it reads as nothing having happened. A failure puts the board
  /// back exactly as it was and says so — the pin is the one signal on this
  /// board whose truth is the server's, so a card left floating on a write
  /// that never landed would be the board lying about the operator's own
  /// instruction.
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

  /// One door to the profile, from wherever an avatar appears.
  const openAgent = useCallback((agentId: string) => {
    setShowActivity(false);
    setProfileOf(agentId);
  }, []);

  const archived = useMemo(() => project?.archived_at_ms != null, [project]);

  if (loading && project === null) {
    return (
      <div className="flex flex-1 items-center justify-center">
        <RiLoader4Line className="text-3xl text-ink-soft animate-spin" />
      </div>
    );
  }

  return (
    <div className="flex flex-col h-full min-h-0">
      <header className="h-12 shrink-0 px-4 border-b-2 border-black flex items-center gap-3 bg-canvas">
        <ProjectSwitcher
          current={project}
          projects={projects}
          refreshKey={refreshKey}
          onCreated={() => {
            setRefreshKey((key) => key + 1);
          }}
        />
        {archived ? (
          <span className="inline-flex items-center gap-1 border-2 border-warn bg-warn/10 text-warn rounded-md px-2 py-0.5 font-mono text-[0.62rem] font-bold uppercase tracking-wider">
            <RiArchiveLine /> Archived — read only
          </span>
        ) : null}
        <TeamStrip
          team={team}
          activeRuns={activeRuns}
          portrait={portrait}
          readOnly={archived}
          hireOpen={hireOpen}
          onHireClosed={() => {
            setHireOpen(false);
          }}
          onOpenProfile={(agent) => {
            openAgent(agent.id);
          }}
          onHire={async (body) => {
            const outcome = await hireAgent(client, projectId, body);
            if (outcome.kind === 'unauthorized') {
              logout();
              return null;
            }
            if (outcome.kind === 'failed') return outcome.message;
            pushToast('ok', `@${outcome.value.handle} joined the project`);
            setRefreshKey((key) => key + 1);
            return null;
          }}
        />
        <div className="ml-auto flex items-center gap-2">
          {project === null ? null : (
            <OverCeilingChip
              project={project}
              activity={activity}
              held={heldCount}
              onOpenSettings={() => {
                setShowSettings(true);
              }}
            />
          )}
          <button
            type="button"
            disabled={unread === 0}
            // No `aria-label`: it would replace the words on the button, so
            // "click Mark read" would stop working by voice and the title
            // below — the only place the reach of the press is spelled out —
            // would go with it. Unlabelled, the name is the button's own
            // text and the title becomes its description.
            title="Clear the unread count on every card on this board — including cards the filter is hiding"
            onClick={() => {
              void markBoardRead();
            }}
            className={`${HEADER_ACTION} ${unread === 0 ? HEADER_ACTION_DEAD : HEADER_ACTION_OFF}`}
          >
            <RiCheckDoubleLine aria-hidden />
            {/* The space is for the accessible name, not for the layout —
                `gap-1` draws the gap and a whitespace-only text node makes no
                flex item. Without it the button announces as "Mark read2". */}
            Mark read{' '}
            {unread > 0 ? (
              // A number the press can empty, so it is allowed to be a
              // number: the rail's own count became a dot precisely because
              // clicking it could not.
              <span className="rounded-full bg-ink text-brand px-1.5 tabular-nums">{unread}</span>
            ) : null}
          </button>
          <BoardFilterMenu
            filter={filter}
            team={team}
            onChange={(next: BoardFilter) => {
              setParams(boardFilterParams(next), { replace: true });
            }}
          />
          <button
            type="button"
            aria-pressed={showActivity}
            onClick={() => {
              setShowActivity((open) => !open);
            }}
            className={`${HEADER_ACTION} ${showActivity ? HEADER_ACTION_ON : HEADER_ACTION_OFF}`}
          >
            Activity
          </button>
          <button
            type="button"
            aria-label="Project settings"
            title="Project settings"
            onClick={() => {
              setShowSettings(true);
            }}
            className={`${HEADER_ACTION} ${HEADER_ACTION_OFF}`}
          >
            {/* Sized against the button, not against the group's label text.
                A 10px word is legible because the word is read whole; a 10px
                glyph is a smudge. 16px keeps the button within a pixel of the
                two beside it, so the group still reads as one row. */}
            <RiSettings3Line aria-hidden className="text-base" />
          </button>
        </div>
      </header>

      {error != null ? (
        <div className="m-4 bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm break-words">
          {error}
        </div>
      ) : null}

      <div className="relative flex-1 min-h-0 flex">
        <DndContext
          sensors={sensors}
          collisionDetection={boardCollisionDetection}
          onDragStart={onDragStart}
          onDragOver={onDragOver}
          onDragEnd={(event) => {
            void onDragEnd(event);
          }}
          onDragCancel={onDragCancel}
        >
          <div className="flex-1 min-h-0 flex gap-3 p-4 overflow-x-auto bg-surface">
            {COLUMNS.map((status) => (
              <BoardColumn
                key={status}
                status={status}
                issues={view[status]}
                total={liveCount(board[status])}
                activeRuns={activeRuns}
                team={team}
                portrait={portrait}
                disabled={archived}
                activeOver={dropInto}
                onOpen={openIssue}
                onOpenAgent={openAgent}
                onTogglePin={togglePin}
                onCreate={() => {
                  setCreateIn(status);
                }}
                onExpand={() => {
                  // The column page reads the same filter params this board
                  // does, so the narrowing survives the zoom in — and the
                  // zoom back out.
                  navigate({
                    pathname: `/projects/${encodeURIComponent(projectId)}/board/${status}`,
                    search: params.toString(),
                  });
                }}
              />
            ))}
          </div>

          <DragOverlay>
            {/* No width here on purpose. dnd-kit sizes the overlay wrapper to
                the rect it measured on the card that was picked up, so `w-full`
                is the picked-up card's own width at every viewport. A literal
                one (this was `w-[200px]`) makes the card snap narrower the
                instant it leaves the column and snap back on release. */}
            {dragging !== null ? <IssueCard issue={dragging} team={team} overlay /> : null}
          </DragOverlay>
        </DndContext>
        {showActivity ? (
          <FloatingPanel
            onDismiss={() => {
              setShowActivity(false);
            }}
          >
            {(leave) => (
              <ActivityDrawer
                projectId={projectId}
                refreshKey={refreshKey}
                onClose={leave}
                onOpenIssue={openIssue}
              />
            )}
          </FloatingPanel>
        ) : null}
        {profileOf !== null && !showActivity
          ? (() => {
              const agent = team.find((row) => row.id === profileOf);
              return agent === undefined ? null : (
                <FloatingPanel
                  // A fresh panel per agent: pressing another avatar while
                  // one is open must switch to it, not have the outgoing
                  // panel's pending unmount close its replacement.
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
                      onChanged={() => {
                        setRefreshKey((key) => key + 1);
                      }}
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
                          setRefreshKey((key) => key + 1);
                        });
                      }}
                    />
                  )}
                </FloatingPanel>
              );
            })()
          : null}
      </div>

      {showSettings && project !== null ? (
        <ProjectSettings
          project={project}
          onClose={() => {
            setShowSettings(false);
          }}
          team={team}
          onOpenProfile={(agent) => {
            setShowSettings(false);
            openAgent(agent.id);
          }}
          onAddAgent={() => {
            setShowSettings(false);
            setHireOpen(true);
          }}
          onSaved={(saved) => {
            setProject(saved);
            // `refetch`, not a bare `setRefreshKey`: raising the ceiling is
            // the one act that releases a held run, and this was the one
            // refresh in the page that did not also ask the rail again — so
            // the dot the operator had just discharged sat there for up to a
            // poll interval and read as one the fix had not worked on.
            refetch();
          }}
        />
      ) : null}

      {createIn !== null ? (
        <CreateIssueModal
          projectId={projectId}
          status={createIn}
          agents={assignableAgents(team)}
          portrait={portrait}
          parents={Object.values(board)
            .flat()
            .filter((row) => row.parent == null && row.cancelled_at_ms == null)
            .map((row) => ({ number: row.number, title: row.title }))}
          onClose={() => {
            setCreateIn(null);
          }}
          onCreated={() => {
            setCreateIn(null);
            setRefreshKey((key) => key + 1);
          }}
        />
      ) : null}

      <ToastStack toasts={toasts} onDismiss={dismissToast} />
    </div>
  );
}

function BoardColumn({
  status,
  issues,
  total,
  activeRuns,
  team,
  portrait,
  disabled,
  activeOver,
  onOpen,
  onOpenAgent,
  onTogglePin,
  onCreate,
  onExpand,
}: {
  status: IssueStatus;
  issues: Issue[];
  total: number;
  activeRuns: IssueRun[];
  team: Agent[];
  portrait: Portrait;
  disabled: boolean;
  /// Which column the dragged card would land in right now, or null when
  /// nothing is being dragged.
  activeOver: IssueStatus | null;
  onOpen: (number: number) => void;
  onOpenAgent: (agentId: string) => void;
  onTogglePin: (number: number, pinned: boolean) => void;
  onCreate: () => void;
  onExpand: () => void;
}) {
  const { setNodeRef, isOver } = useDroppable({ id: columnDropId(status) });
  // One array per set of cards rather than per render: `SortableContext` keys
  // its whole context value off this, so a fresh one each render re-renders
  // every card in the column, re-runs its layout effects, and permanently
  // disables the sort animation (it compares the array by identity).
  const items = useMemo(() => issues.map((issue) => cardDragId(issue.number)), [issues]);
  // `isOver` only fires for the column's own droppable — hovering a card
  // inside it does not. `activeOver` is what the operator experiences as
  // "this is where it will land", so the outline follows that instead.
  //
  // This says *which column*, and that is all it says. Where in the column is
  // already answered, exactly, by the dragged card itself: `onDragOver` moves
  // it into place and it renders there at 40% while the drag is live. A dashed
  // slot used to sit under the list saying the same thing less truthfully — it
  // was pinned to the column's end whatever the real insertion point was.
  const targeted = isOver || activeOver === status;

  return (
    // The droppable is the whole column, header and borders included, not just
    // the list under them. A cursor is over a droppable or it is over nothing
    // (see `boardCollisionDetection`), and with the list alone answering, the
    // full-width band across every column header was board that took no card:
    // dragging to the top of a column — where the operator aims when they want
    // it first — landed on a pixel with no answer.
    <section
      ref={setNodeRef}
      className={`flex-1 min-w-[210px] flex flex-col border-2 rounded-md bg-canvas max-h-full ${
        targeted ? 'border-brand-hover shadow-brutal-sm' : 'border-black'
      }`}
    >
      <header className="flex items-center gap-2 px-2.5 py-2 border-b-2 border-black shrink-0">
        <h2 className="font-mono text-[0.68rem] font-bold uppercase tracking-wider">
          {COLUMN_LABEL[status]}
        </h2>
        <span
          className="rounded-full bg-ink text-brand font-mono text-[0.58rem] px-2 leading-[1.15rem] tabular-nums"
          title={
            liveCount(issues) === total
              ? `${total} live`
              : `${liveCount(issues)} of ${total} live cards match the filter`
          }
        >
          {liveCount(issues) === total ? total : `${liveCount(issues)}/${total}`}
        </span>
        {/* Not disabled with the board: maximizing is reading, and an
            archived board is exactly the one being read rather than worked. */}
        <IconButton
          className="ml-auto !w-6 !h-6 bg-surface"
          title={`Open ${COLUMN_LABEL[status]} as a full page`}
          onClick={onExpand}
        >
          <RiFullscreenLine />
        </IconButton>
        <IconButton
          className="!w-6 !h-6 bg-surface"
          title={`New issue in ${COLUMN_LABEL[status]}`}
          onClick={onCreate}
          disabled={disabled}
        >
          <RiAddLine />
        </IconButton>
      </header>
      <div
        className={`flex-1 min-h-[70px] overflow-y-auto overscroll-none flex flex-col gap-2 p-2 ${
          targeted ? 'bg-brand/15' : ''
        }`}
      >
        <SortableContext items={items} strategy={verticalListSortingStrategy}>
          {issues.map((issue) => (
            <SortableIssueCard
              key={issue.number}
              issue={issue}
              run={runIndicator(activeRuns, issue.number)}
              team={team}
              portrait={portrait}
              disabled={disabled}
              onOpen={onOpen}
              onOpenAgent={onOpenAgent}
              onTogglePin={onTogglePin}
            />
          ))}
        </SortableContext>
        {issues.length === 0 ? (
          <p className="m-auto text-center font-mono text-[0.62rem] text-ink-soft leading-snug">
            {total === 0 ? (
              <>
                No issues
                <br />
                Drag one in, or use the column’s +
              </>
            ) : (
              'No matches'
            )}
          </p>
        ) : null}
      </div>
    </section>
  );
}

function SortableIssueCard({
  issue,
  run,
  team,
  portrait,
  disabled,
  onOpen,
  onOpenAgent,
  onTogglePin,
}: {
  issue: Issue;
  run: AvatarRun;
  team: Agent[];
  portrait: Portrait;
  disabled: boolean;
  onOpen: (number: number) => void;
  onOpenAgent: (agentId: string) => void;
  onTogglePin: (number: number, pinned: boolean) => void;
}) {
  const { attributes, listeners, setNodeRef, setActivatorNodeRef, transform, transition, isDragging } =
    useSortable({
      id: cardDragId(issue.number),
      disabled,
    });

  return (
    <div
      // The wrapper is also the keyboard sensor's activator node, so its
      // `event.target !== activator` guard engages: Enter on a control
      // inside the card — the avatar, the pin, the branch chip — presses
      // that control instead of silently lifting the card.
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
      <IssueCard
        issue={issue}
        run={run}
        team={team}
        portrait={portrait}
        disabled={disabled}
        onOpenAgent={onOpenAgent}
        onTogglePin={onTogglePin}
      />
    </div>
  );
}

function IssueCard({
  issue,
  run = null,
  overlay = false,
  team = [],
  portrait = generatedPortrait,
  disabled = false,
  onOpenAgent,
  onTogglePin,
}: {
  issue: Issue;
  run?: AvatarRun;
  overlay?: boolean;
  team?: Agent[];
  /// Resolved faces from the board. The drag overlay leaves it alone and
  /// draws the generated one, which is the same picture in every case an
  /// upload is absent — and a drag is too short to fetch one anyway.
  portrait?: Portrait;
  /// An archived board. Its cards are read-only, so the pin is shown as it
  /// is and cannot be worked.
  disabled?: boolean;
  /// Opens the assignee's profile. Absent on the drag overlay, which is a
  /// picture of a card rather than a card.
  onOpenAgent?: (agentId: string) => void;
  /// Absent on the drag overlay, for the same reason.
  onTogglePin?: (number: number, pinned: boolean) => void;
}) {
  const cancelled = issue.cancelled_at_ms != null;
  const priority = PRIORITY_MARK[issue.priority];
  return (
    <article
      className={`group bg-surface border-2 border-black rounded-md shadow-brutal-xs px-2.5 py-2 flex flex-col gap-1.5 cursor-pointer ${
        overlay ? 'rotate-2 shadow-brutal w-full h-full' : ''
      } ${cancelled ? 'opacity-55' : ''}`}
    >
      <div className="flex items-center gap-1.5 font-mono text-[0.6rem] text-ink-soft">
        {priority !== null ? (
          <span className={`${priority.tone} font-bold`}>{priority.glyph}</span>
        ) : null}
        <span className="font-bold">#{issue.number}</span>
        {/* In front of the time, at the row's other end: the head of the meta
            row is where the card is identified — its mark and its number —
            and a press standing in that queue reads as one more fact about
            the card. The corner past the time stays the unread count's, the
            one number on this board the eye is trained to find. */}
        <span className="ml-auto flex items-center gap-1.5">
          {onTogglePin === undefined ? null : (
            <PinButton
              pinned={issue.pinned}
              disabled={disabled}
              onToggle={(pinned) => {
                onTogglePin(issue.number, pinned);
              }}
            />
          )}
          <span
            className="tabular-nums"
            title={`Last touched ${new Date(issue.updated_at_ms).toLocaleString()}`}
          >
            {updatedAgo(issue.updated_at_ms, Date.now())}
          </span>
        </span>
        {issue.unread > 0 ? <UnreadPill unread={issue.unread} /> : null}
      </div>
      <p
        className={`font-mono text-[0.76rem] font-bold leading-snug line-clamp-2 ${
          cancelled ? 'line-through' : ''
        }`}
      >
        {issue.title}
      </p>
      {issue.blocked_reason != null ? <BlockedBadge reason={issue.blocked_reason} /> : null}
      {issue.last_run_failed ? <FailedBadge /> : null}
      {hasDeliverable(issue) && issue.branch != null ? <BranchChip branch={issue.branch} /> : null}
      <div className="flex items-center gap-1.5">
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
        {issue.sub_issues != null ? (
          <span className="ml-auto">
            <SubIssueRing progress={issue.sub_issues} />
          </span>
        ) : null}
        <RunWord run={run} className={issue.sub_issues == null ? 'ml-auto' : ''} />
      </div>
    </article>
  );
}
