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
  liveCount,
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
  const { toasts, pushToast } = useToasts();

  const [project, setProject] = useState<Project | null>(null);
  const [projects, setProjects] = useState<Project[]>([]);
  const [team, setTeam] = useState<Agent[]>([]);
  const [activeRuns, setActiveRuns] = useState<IssueRun[]>([]);
  const [board, setBoard] = useState<Board>(emptyBoard);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);
  const [dragging, setDragging] = useState<Issue | null>(null);
  const [createIn, setCreateIn] = useState<IssueStatus | null>(null);
  const [params, setParams] = useSearchParams();
  const filter = useMemo(() => parseBoardFilter(params), [params]);
  const [rightPanel, setRightPanel] = useState<'activity' | 'lead' | null>(null);
  const [profileOf, setProfileOf] = useState<string | null>(null);
  const [showSettings, setShowSettings] = useState(false);

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
        return drop === null ? current : moveCard(current, drop);
      });
    },
    [filter, team],
  );

  const onDragEnd = useCallback(
    async (event: DragEndEvent) => {
      setDragging(null);
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
        pushToast('err', outcome.message);
        return;
      }
      pushToast('ok', `#${number} → ${COLUMN_LABEL[issue.status]}`);
    },
    [board, client, filter, logout, projectId, pushToast, team],
  );

  const onDragCancel = useCallback(() => {
    setDragging(null);
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
                onOpen={openIssue}
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
              readOnly={archived}
              onClose={() => {
                setRightPanel(null);
              }}
              onBoardChanged={() => {
                setRefreshKey((key) => key + 1);
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
          onClose={() => {
            setCreateIn(null);
          }}
          onCreated={() => {
            setCreateIn(null);
            setRefreshKey((key) => key + 1);
          }}
        />
      ) : null}

      <ToastStack toasts={toasts} />
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
  onOpen,
  onCreate,
}: {
  status: IssueStatus;
  issues: Issue[];
  total: number;
  activeRuns: IssueRun[];
  team: Agent[];
  disabled: boolean;
  onOpen: (number: number) => void;
  onCreate: () => void;
}) {
  const { setNodeRef, isOver } = useDroppable({ id: columnDropId(status) });

  return (
    <section className="flex-1 min-w-[210px] flex flex-col border-2 border-black rounded-md bg-canvas max-h-full">
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
          isOver ? 'bg-brand/15' : ''
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
            />
          ))}
        </SortableContext>
        {issues.length === 0 ? (
          <p className="m-auto text-center font-mono text-[0.62rem] text-ink-soft leading-snug">
            {total === 0 ? 'No issues' : 'No matches'}
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
}: {
  issue: Issue;
  run: 'queued' | 'running' | null;
  team: Agent[];
  disabled: boolean;
  onOpen: (number: number) => void;
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
      <IssueCard issue={issue} run={run} team={team} />
    </div>
  );
}

function SubIssueRing({ progress }: { progress: { done: number; total: number } }) {
  const { done, total } = progress;
  const pct = total === 0 ? 0 : Math.round((done / total) * 100);
  return (
    <div
      className="flex items-center gap-1.5"
      title={`${done} of ${total} sub-issues done`}
    >
      <div className="flex-1 h-1 border border-black/25 rounded-full overflow-hidden bg-canvas">
        <div
          className={`h-full ${done === total ? 'bg-ok' : 'bg-brand'}`}
          style={{ width: `${pct}%` }}
        />
      </div>
      <span className="font-mono text-[0.54rem] text-ink-soft tabular-nums">
        {done}/{total}
      </span>
    </div>
  );
}

function FloatingPanel({ children }: { children: React.ReactNode }) {
  return (
    <div className="absolute inset-y-0 right-0 z-30 flex shadow-brutal">{children}</div>
  );
}

function AssigneeDot({ assignee, working = false }: { assignee: string; working?: boolean }) {
  return (
    <span
      title={working ? `${assignee} — working` : assignee}
      className={`w-4 h-4 rounded-full border-2 border-black shrink-0 ${
        working ? 'bg-ok motion-safe:animate-pulse' : 'bg-brand'
      }`}
    />
  );
}

function IssueCard({
  issue,
  run = null,
  overlay = false,
  team = [],
}: {
  issue: Issue;
  run?: 'queued' | 'running' | null;
  overlay?: boolean;
  team?: Agent[];
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
      {issue.sub_issues != null ? <SubIssueRing progress={issue.sub_issues} /> : null}
      {issue.blocked_reason != null ? (
        <span className="self-start border border-warn/50 bg-warn/10 text-warn rounded px-1.5 font-mono text-[0.56rem] font-bold uppercase">
          ⚑ Blocked
        </span>
      ) : null}
      {issue.branch != null ? (
        <span
          className="self-start max-w-full truncate border border-black/25 bg-canvas rounded px-1.5 font-mono text-[0.56rem] text-ink-soft"
          title={issue.branch}
        >
          ⑂ {issue.branch}
        </span>
      ) : null}
      {issue.assignee != null ? (
        <div className="flex items-center gap-1.5">
          <AssigneeDot assignee={handleOf(team, issue.assignee)} working={run === 'running'} />
          <span className="font-mono text-[0.58rem] text-ink-soft truncate">
            @{handleOf(team, issue.assignee)}
          </span>
          {run !== null ? (
            <span
              className={`ml-auto shrink-0 font-mono text-[0.54rem] font-bold uppercase ${
                run === 'running' ? 'text-ok' : 'text-ink-soft'
              }`}
            >
              {run === 'running' ? 'working' : 'queued'}
            </span>
          ) : null}
        </div>
      ) : null}
    </article>
  );
}
