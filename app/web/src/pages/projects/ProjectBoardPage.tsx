import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useNavigate, useParams, useSearchParams } from 'react-router-dom';
import {
  DndContext,
  DragOverlay,
  KeyboardSensor,
  PointerSensor,
  closestCorners,
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
import { RiAddLine, RiArchiveLine, RiLoader4Line } from 'react-icons/ri';

import { useAdminClient, useAuth } from '../../api/auth';
import { IconButton } from '../../components/IconButton';
import {
  fetchActiveRuns,
  fetchTeam,
  hireAgent,
  markProjectRead,
  removeAgent,
  fetchIssues,
  fetchProject,
  fetchProjects,
  moveIssue,
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
  cardDragId,
  assignableAgents,
  columnDropId,
  dropRejection,
  emptyBoard,
  findIssue,
  groupByStatus,
  hasDeliverable,
  liveCount,
  moveAnnouncement,
  statusOf,
  updatedAgo,
  moveCard,
  orderedNumbers,
  parseDragId,
  placementChanged,
  resolveDrop,
  runIndicator,
} from './boardModel';
import { writeLastProjectId } from './lastProject';
import { CreateIssueModal } from './CreateIssueModal';
import { ProjectSwitcher } from './ProjectSwitcher';
import { ActivityDrawer } from './ActivityDrawer';
import { AgentProfile } from './AgentProfile';
import { LeadPanel } from './LeadPanel';
import { ProjectSettings } from './ProjectSettings';
import { BoardFilterBar } from './BoardFilterBar';
import { boardFilterParams, filterBoard, parseBoardFilter } from './boardFilter';
import type { BoardFilter } from './boardFilter';
import { TeamStrip } from './TeamStrip';
import { handleOf } from './teamModel';
import { ToastStack, useToasts } from './Toasts';
import { useBoardStream } from './useBoardStream';

const PRIORITY_MARK: Record<Issue['priority'], { glyph: string; tone: string } | null> = {
  urgent: { glyph: '▲▲', tone: 'text-err' },
  high: { glyph: '▲', tone: 'text-warn' },
  medium: { glyph: '◆', tone: 'text-info' },
  low: { glyph: '▽', tone: 'text-ink-soft' },
  none: null,
};

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
  const [rightPanel, setRightPanel] = useState<'activity' | 'lead' | null>(null);
  const [profileOf, setProfileOf] = useState<string | null>(null);
  const [showSettings, setShowSettings] = useState(false);
  const [hireOpen, setHireOpen] = useState(false);

  // onDragOver mutates the preview, so rollback needs the drag-start board.
  const preDrag = useRef<Board | null>(null);

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
      setBoard(groupByStatus(issuesOutcome.value));
      if (listOutcome.kind === 'ok') setProjects(listOutcome.value);
      if (agentsOutcome.kind === 'ok') setTeam(agentsOutcome.value);
      setActiveRuns(runsOutcome.kind === 'ok' ? runsOutcome.value : []);
      setLoading(false);
      writeLastProjectId(projectId);
      void markProjectRead(client, projectId);
    }
    void load();
    return () => {
      canceled = true;
    };
  }, [client, logout, projectId, refreshKey]);

  // Move requests use the full column order, never this filtered view.
  const view = useMemo(() => filterBoard(board, filter, team), [board, filter, team]);

  useBoardStream(
    projectId,
    null,
    useCallback(() => {
      setRefreshKey((key) => key + 1);
    }, []),
  );

  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 4 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );

  const onDragStart = useCallback(
    (event: DragStartEvent) => {
      preDrag.current = board;
      const target = parseDragId(String(event.active.id));
      setDragging(target?.kind === 'card' ? findIssue(board, target.number) : null);
    },
    [board],
  );

  // dnd-kit needs cross-column previews here to open the destination gap.
  const onDragOver = useCallback(
    (event: DragOverEvent) => {
      const activeId = String(event.active.id);
      const overId = event.over === null ? null : String(event.over.id);
      setBoard((current) => {
        const drop = resolveDrop(filterBoard(current, filter, team), activeId, overId);
        if (drop !== null) setDropInto(drop.status);
        return drop === null ? current : moveCard(current, drop);
      });
    },
    [filter, team],
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
        filterBoard(board, filter, team),
        String(event.active.id),
        event.over === null ? null : String(event.over.id),
      );
      const next = drop === null ? board : moveCard(board, drop);
      if (!placementChanged(before, next, number)) {
        setBoard(before);
        return;
      }
      setBoard(next);

      const issue = findIssue(next, number);
      if (issue === null) return;
      const refusal = dropRejection(issue, issue.status);
      if (refusal !== null) {
        setBoard(before);
        pushToast('warn', refusal);
        return;
      }
      const from = statusOf(before, number);
      const outcome = await moveIssue(
        client,
        projectId,
        number,
        issue.status,
        orderedNumbers(next, issue.status),
      );
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setBoard(before);
        pushToast('err', `Move failed, rolled back — ${outcome.message}`, {
          label: 'Retry',
          run: () => {
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
    [board, client, filter, logout, projectId, pushToast, team],
  );

  const onDragCancel = useCallback(() => {
    setDragging(null);
    setDropInto(null);
    const before = preDrag.current;
    preDrag.current = null;
    if (before !== null) setBoard(before);
  }, []);

  const openIssue = useCallback(
    (number: number) => {
      navigate(`/projects/${encodeURIComponent(projectId)}/issues/${number}`);
    },
    [navigate, projectId],
  );

  /// One door to the profile, from wherever an avatar appears.
  const openAgent = useCallback((agentId: string) => {
    setRightPanel(null);
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
          readOnly={archived}
          hireOpen={hireOpen}
          onHireClosed={() => {
            setHireOpen(false);
          }}
          onOpenProfile={(agent) => {
            setRightPanel(null);
            setProfileOf(agent.id);
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
          onRemove={(agent) => {
            void removeAgent(client, projectId, agent.id).then((outcome) => {
              if (outcome.kind === 'unauthorized') {
                logout();
                return;
              }
              pushToast(
                outcome.kind === 'ok' ? 'ok' : 'err',
                outcome.kind === 'ok' ? `@${agent.handle} left the project` : outcome.message,
              );
              setRefreshKey((key) => key + 1);
            });
          }}
        />
        <div className="ml-auto flex items-center gap-2">
          <button
            type="button"
            aria-pressed={rightPanel === 'lead'}
            onClick={() => {
              setRightPanel((open) => (open === 'lead' ? null : 'lead'));
            }}
            className={`border-2 border-black rounded-md px-2 py-0.5 font-mono text-[0.62rem] font-bold uppercase tracking-wider ${
              rightPanel === 'lead' ? 'bg-brand text-ink' : 'bg-surface'
            }`}
          >
            Chat with lead
          </button>
          <button
            type="button"
            aria-pressed={rightPanel === 'activity'}
            onClick={() => {
              setRightPanel((open) => (open === 'activity' ? null : 'activity'));
            }}
            className={`border-2 border-black rounded-md px-2 py-0.5 font-mono text-[0.62rem] font-bold uppercase tracking-wider ${
              rightPanel === 'activity' ? 'bg-brand text-ink' : 'bg-surface'
            }`}
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
            className="border-2 border-black rounded-md bg-surface px-2 py-0.5 font-mono text-[0.62rem] font-bold"
          >
            ⚙
          </button>
        </div>
      </header>

      <BoardFilterBar
        filter={filter}
        team={team}
        onChange={(next: BoardFilter) => {
          setParams(boardFilterParams(next), { replace: true });
        }}
      />

      {error != null ? (
        <div className="m-4 bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm break-words">
          {error}
        </div>
      ) : null}

      <div className="relative flex-1 min-h-0 flex">
        <DndContext
          sensors={sensors}
          collisionDetection={closestCorners}
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
                disabled={archived}
                activeOver={dropInto}
                onOpen={openIssue}
                onOpenAgent={openAgent}
                onCreate={() => {
                  setCreateIn(status);
                }}
              />
            ))}
          </div>

          <DragOverlay>
            {dragging !== null ? <IssueCard issue={dragging} team={team} overlay /> : null}
          </DragOverlay>
        </DndContext>
        {rightPanel === 'activity' ? (
          <FloatingPanel>
            <ActivityDrawer
              projectId={projectId}
              refreshKey={refreshKey}
              onClose={() => {
                setRightPanel(null);
              }}
              onOpenIssue={openIssue}
            />
          </FloatingPanel>
        ) : null}
        {profileOf !== null && rightPanel === null
          ? (() => {
              const agent = team.find((row) => row.id === profileOf);
              return agent === undefined ? null : (
                <FloatingPanel>
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
                    onChatWithLead={
                      agent.lead
                        ? () => {
                            setProfileOf(null);
                            setRightPanel('lead');
                          }
                        : undefined
                    }
                    onClose={() => {
                      setProfileOf(null);
                    }}
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
                </FloatingPanel>
              );
            })()
          : null}
        {rightPanel === 'lead' ? (
          <FloatingPanel>
            <LeadPanel
              projectId={projectId}
              projectName={project?.name ?? 'this board'}
              readOnly={archived}
              onClose={() => {
                setRightPanel(null);
              }}
              onBoardChanged={() => {
                setRefreshKey((key) => key + 1);
              }}
              onHireLead={() => {
                setRightPanel(null);
                setHireOpen(true);
              }}
            />
          </FloatingPanel>
        ) : null}
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
            setRefreshKey((key) => key + 1);
          }}
        />
      ) : null}

      {createIn !== null ? (
        <CreateIssueModal
          projectId={projectId}
          status={createIn}
          agents={assignableAgents(team)}
          parents={Object.values(board)
            .flat()
            .filter((row) => row.parent == null && row.cancelled_at_ms == null)
            .map((row) => ({ number: row.number, title: row.title }))}
          onAskLead={() => {
            setCreateIn(null);
            setRightPanel('lead');
          }}
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
  disabled,
  activeOver,
  onOpen,
  onOpenAgent,
  onCreate,
}: {
  status: IssueStatus;
  issues: Issue[];
  total: number;
  activeRuns: IssueRun[];
  team: Agent[];
  disabled: boolean;
  /// Which column the dragged card would land in right now, or null when
  /// nothing is being dragged.
  activeOver: IssueStatus | null;
  onOpen: (number: number) => void;
  onOpenAgent: (agentId: string) => void;
  onCreate: () => void;
}) {
  const { setNodeRef, isOver } = useDroppable({ id: columnDropId(status) });
  // `isOver` only fires for the column's own droppable — hovering a card
  // inside it does not. `activeOver` is what the operator experiences as
  // "this is where it will land", so the outline follows that instead.
  const targeted = isOver || activeOver === status;

  return (
    <section
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
        <IconButton
          className="ml-auto !w-6 !h-6 bg-surface"
          title={`New issue in ${COLUMN_LABEL[status]}`}
          onClick={onCreate}
          disabled={disabled}
        >
          <RiAddLine />
        </IconButton>
      </header>
      <div
        ref={setNodeRef}
        className={`flex-1 min-h-[70px] overflow-y-auto flex flex-col gap-2 p-2 ${
          targeted ? 'bg-brand/15' : ''
        }`}
      >
        <SortableContext
          items={issues.map((issue) => cardDragId(issue.number))}
          strategy={verticalListSortingStrategy}
        >
          {issues.map((issue) => (
            <SortableIssueCard
              key={issue.number}
              issue={issue}
              run={runIndicator(activeRuns, issue.number)}
              team={team}
              disabled={disabled}
              onOpen={onOpen}
              onOpenAgent={onOpenAgent}
            />
          ))}
        </SortableContext>
        {targeted ? (
          // The dashed slot: where the card lands if it is let go now.
          <div className="shrink-0 h-9 border-2 border-dashed border-brand-hover rounded-md" />
        ) : null}
        {issues.length === 0 && !targeted ? (
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
  disabled,
  onOpen,
  onOpenAgent,
}: {
  issue: Issue;
  run: 'queued' | 'running' | null;
  team: Agent[];
  disabled: boolean;
  onOpen: (number: number) => void;
  onOpenAgent: (agentId: string) => void;
}) {
  const { attributes, listeners, setNodeRef, transform, transition, isDragging } = useSortable({
    id: cardDragId(issue.number),
    disabled,
  });

  return (
    <div
      ref={setNodeRef}
      style={{ transform: CSS.Transform.toString(transform), transition }}
      className={`touch-none ${isDragging ? 'opacity-40' : ''}`}
      {...attributes}
      {...listeners}
      onClick={() => {
        onOpen(issue.number);
      }}
    >
      <IssueCard issue={issue} run={run} team={team} onOpenAgent={onOpenAgent} />
    </div>
  );
}

/// A ring, not a bar. It sits inline beside the assignee on a card whose
/// other rows are already full-width, and a circle reads as "how far
/// through the steps" at a glance where a fifth horizontal bar would just
/// be another line of card furniture.
function SubIssueRing({ progress }: { progress: { done: number; total: number } }) {
  const { done, total } = progress;
  const fraction = total === 0 ? 0 : done / total;
  const radius = 5;
  const circumference = 2 * Math.PI * radius;
  return (
    <span
      className="inline-flex items-center gap-1 shrink-0"
      title={`${done} of ${total} sub-issues done`}
    >
      <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true" className="shrink-0">
        <circle
          cx="7"
          cy="7"
          r={radius}
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          className="text-black/20"
        />
        <circle
          cx="7"
          cy="7"
          r={radius}
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          strokeDasharray={`${circumference * fraction} ${circumference}`}
          transform="rotate(-90 7 7)"
          className={done === total ? 'text-ok' : 'text-brand'}
        />
      </svg>
      <span className="font-mono text-[0.54rem] text-ink-soft tabular-nums">
        {done}/{total}
      </span>
    </span>
  );
}

function FloatingPanel({ children }: { children: React.ReactNode }) {
  return (
    <div className="absolute inset-y-0 right-0 z-30 flex shadow-brutal">{children}</div>
  );
}

/// The three states the mockup's avatar has: shimmering while a run is
/// executing, **dimmed** while one is only queued, and plain otherwise.
/// Queued had no look of its own before, so a card waiting its turn was
/// pixel-identical to one nobody had started.
function AssigneeDot({
  assignee,
  run = null,
}: {
  assignee: string;
  run?: 'queued' | 'running' | null;
}) {
  const title =
    run === 'running'
      ? `${assignee} — working`
      : run === 'queued'
        ? `${assignee} — queued, waiting for a free slot`
        : assignee;
  return (
    <span
      title={title}
      className={`w-4 h-4 rounded-full border-2 border-black shrink-0 ${
        run === 'running' ? 'bg-ok motion-safe:animate-pulse' : 'bg-brand'
      } ${run === 'queued' ? 'opacity-40' : ''}`}
    />
  );
}

/// The branch chip. Copies rather than navigates: it sits inside a card
/// whose own click opens the issue, so without stopping the event the one
/// affordance the mockup gives it would be unreachable.
function BranchChip({ branch }: { branch: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      title={`${branch} — click to copy`}
      className="self-start max-w-full truncate border border-black/25 bg-canvas rounded px-1.5 font-mono text-[0.56rem] text-ink-soft cursor-pointer hover:border-black"
      onClick={(event) => {
        event.stopPropagation();
        try {
          // `navigator.clipboard` is typed as always present but is
          // **absent** outside a secure context — which a dashboard served
          // over plain http on a LAN address is. Reading `.writeText` off
          // `undefined` throws synchronously, so the try is the guard; a
          // rejected promise (permission refused) is the other half.
          void navigator.clipboard.writeText(branch).then(
            () => {
              setCopied(true);
              window.setTimeout(() => {
                setCopied(false);
              }, 1200);
            },
            () => {
              // Nothing: the branch name is still in the title attribute,
              // and a "copied" that did not happen is worse than silence.
            },
          );
        } catch {
          // Same reasoning.
        }
      }}
    >
      ⑂ {copied ? 'copied' : branch}
    </button>
  );
}

function IssueCard({
  issue,
  run = null,
  overlay = false,
  team = [],
  onOpenAgent,
}: {
  issue: Issue;
  run?: 'queued' | 'running' | null;
  overlay?: boolean;
  team?: Agent[];
  /// Opens the assignee's profile. Absent on the drag overlay, which is a
  /// picture of a card rather than a card.
  onOpenAgent?: (agentId: string) => void;
}) {
  const cancelled = issue.cancelled_at_ms != null;
  const priority = PRIORITY_MARK[issue.priority];
  return (
    <article
      className={`bg-surface border-2 border-black rounded-md shadow-brutal-xs px-2.5 py-2 flex flex-col gap-1.5 cursor-pointer ${
        overlay ? 'rotate-2 shadow-brutal w-[200px]' : ''
      } ${cancelled ? 'opacity-55' : ''}`}
    >
      <div className="flex items-center gap-1.5 font-mono text-[0.6rem] text-ink-soft">
        {priority !== null ? (
          <span className={`${priority.tone} font-bold`}>{priority.glyph}</span>
        ) : null}
        <span className="font-bold">#{issue.number}</span>
        <span
          className="ml-auto tabular-nums"
          title={`Last touched ${new Date(issue.updated_at_ms).toLocaleString()}`}
        >
          {updatedAgo(issue.updated_at_ms, Date.now())}
        </span>
      </div>
      <p
        className={`font-mono text-[0.76rem] font-bold leading-snug line-clamp-2 ${
          cancelled ? 'line-through' : ''
        }`}
      >
        {issue.title}
      </p>
      {issue.blocked_reason != null ? (
        <span
          className="self-start border border-warn/50 bg-warn/10 text-warn rounded px-1.5 font-mono text-[0.56rem] font-bold uppercase"
          title={issue.blocked_reason}
        >
          ⚑ Blocked
        </span>
      ) : null}
      {hasDeliverable(issue) && issue.branch != null ? <BranchChip branch={issue.branch} /> : null}
      <div className="flex items-center gap-1.5">
        {issue.assignee != null ? (
          <button
            type="button"
            title={`Open @${handleOf(team, issue.assignee)}'s profile`}
            onClick={(event) => {
              // The card's own click opens the issue, so the avatar has to
              // claim the event or it can never be the profile's entry point.
              event.stopPropagation();
              onOpenAgent?.(issue.assignee ?? '');
            }}
            className="flex items-center gap-1.5 min-w-0 cursor-pointer hover:underline"
          >
            <AssigneeDot assignee={handleOf(team, issue.assignee)} run={run} />
            <span className="font-mono text-[0.58rem] text-ink-soft truncate">
              @{handleOf(team, issue.assignee)}
            </span>
          </button>
        ) : (
          // A card nobody is on says so. Rendering nothing made an
          // untriaged card look the same as one whose footer had simply
          // scrolled off, and the parking lot is exactly where the
          // operator is scanning for work to hand out.
          <>
            <span className="w-4 h-4 rounded-full border-2 border-dashed border-black/40 shrink-0" />
            <span className="font-mono text-[0.58rem] text-ink-soft italic">unassigned</span>
          </>
        )}
        {issue.sub_issues != null ? (
          <span className="ml-auto">
            <SubIssueRing progress={issue.sub_issues} />
          </span>
        ) : null}
        {run !== null ? (
          <span
            className={`${issue.sub_issues == null ? 'ml-auto' : ''} shrink-0 font-mono text-[0.54rem] font-bold uppercase ${
              run === 'running' ? 'text-ok' : 'text-ink-soft'
            }`}
          >
            {run === 'running' ? 'working' : 'queued'}
          </span>
        ) : null}
      </div>
    </article>
  );
}
