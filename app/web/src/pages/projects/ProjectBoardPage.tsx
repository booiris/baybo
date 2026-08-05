import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
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
import { fetchAgents, fetchIssues, fetchProject, fetchProjects, moveIssue } from './api';
import {
  COLUMNS,
  COLUMN_LABEL,
  type Agent,
  type Board,
  type Issue,
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
  moveCard,
  orderedNumbers,
  parseDragId,
  placementChanged,
  resolveDrop,
} from './boardModel';
import { writeLastProjectId } from './lastProject';
import { CreateIssueModal } from './CreateIssueModal';
import { ProjectSwitcher } from './ProjectSwitcher';
import { ToastStack, useToasts } from './Toasts';

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
  const [agents, setAgents] = useState<Agent[]>([]);
  const [board, setBoard] = useState<Board>(emptyBoard);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);
  const [dragging, setDragging] = useState<Issue | null>(null);
  const [createIn, setCreateIn] = useState<IssueStatus | null>(null);
  const [showCancelled, setShowCancelled] = useState(false);

  // The board dnd-kit shows mid-drag is already mutated by `onDragOver`, so
  // a rollback needs the board as it was before the drag started.
  const preDrag = useRef<Board | null>(null);

  useEffect(() => {
    let canceled = false;
    async function load() {
      setLoading(true);
      const [projectOutcome, issuesOutcome, listOutcome, agentsOutcome] = await Promise.all([
        fetchProject(client, projectId),
        fetchIssues(client, projectId),
        fetchProjects(client, false),
        fetchAgents(client),
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
      setBoard(groupByStatus(issuesOutcome.value, showCancelled));
      if (listOutcome.kind === 'ok') setProjects(listOutcome.value);
      if (agentsOutcome.kind === 'ok') setAgents(assignableAgents(agentsOutcome.value));
      setLoading(false);
      // Remembered only once the board actually resolved: a 404'd deep link
      // must not poison what the rail opens next time.
      writeLastProjectId(projectId);
    }
    void load();
    return () => {
      canceled = true;
    };
  }, [client, logout, projectId, refreshKey, showCancelled]);

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

  // dnd-kit only opens a gap in a column whose SortableContext actually
  // holds the dragged id, so the cross-column move has to be applied while
  // the pointer is still down — not on drop.
  const onDragOver = useCallback((event: DragOverEvent) => {
    setBoard((current) => {
      const drop = resolveDrop(
        current,
        String(event.active.id),
        event.over === null ? null : String(event.over.id),
      );
      return drop === null ? current : moveCard(current, drop);
    });
  }, []);

  const onDragEnd = useCallback(
    async (event: DragEndEvent) => {
      setDragging(null);
      const before = preDrag.current;
      preDrag.current = null;
      if (before === null) return;

      const target = parseDragId(String(event.active.id));
      if (target?.kind !== 'card') return;
      const number = target.number;

      // `board` here is the preview `onDragOver` built; resolve once more so
      // a drop straight onto a card lands in that card's slot.
      const drop = resolveDrop(
        board,
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
        // Bounce before the request: the server refuses this too, but the
        // card should snap back rather than flicker through the column.
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
    [board, client, logout, projectId, pushToast],
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
        <div className="ml-auto flex items-center gap-2">
          <label className="flex items-center gap-1.5 font-mono text-[0.68rem] text-ink-soft cursor-pointer">
            <input
              type="checkbox"
              checked={showCancelled}
              onChange={(event) => {
                setShowCancelled(event.target.checked);
              }}
            />
            Show cancelled
          </label>
        </div>
      </header>

      {error != null ? (
        <div className="m-4 bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm break-words">
          {error}
        </div>
      ) : null}

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
              issues={board[status]}
              disabled={archived}
              onOpen={openIssue}
              onCreate={() => {
                setCreateIn(status);
              }}
            />
          ))}
        </div>

        <DragOverlay>
          {dragging !== null ? <IssueCard issue={dragging} overlay /> : null}
        </DragOverlay>
      </DndContext>

      {createIn !== null ? (
        <CreateIssueModal
          projectId={projectId}
          status={createIn}
          agents={agents}
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
  disabled,
  onOpen,
  onCreate,
}: {
  status: IssueStatus;
  issues: Issue[];
  disabled: boolean;
  onOpen: (number: number) => void;
  onCreate: () => void;
}) {
  // A plain droppable on the column *body*: it has no sortable identity of
  // its own, and it is what makes an empty column reachable — an empty
  // SortableContext has no items to collide with.
  const { setNodeRef, isOver } = useDroppable({ id: columnDropId(status) });

  return (
    <section className="flex-1 min-w-[210px] flex flex-col border-2 border-black rounded-md bg-canvas max-h-full">
      <header className="flex items-center gap-2 px-2.5 py-2 border-b-2 border-black shrink-0">
        <h2 className="font-mono text-[0.68rem] font-bold uppercase tracking-wider">
          {COLUMN_LABEL[status]}
        </h2>
        <span className="rounded-full bg-ink text-brand font-mono text-[0.58rem] px-2 leading-[1.15rem]">
          {liveCount(issues)}
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
              disabled={disabled}
              onOpen={onOpen}
            />
          ))}
        </SortableContext>
        {issues.length === 0 ? (
          <p className="m-auto text-center font-mono text-[0.62rem] text-ink-soft leading-snug">
            No issues
          </p>
        ) : null}
      </div>
    </section>
  );
}

function SortableIssueCard({
  issue,
  disabled,
  onOpen,
}: {
  issue: Issue;
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
      <IssueCard issue={issue} />
    </div>
  );
}

/** Initial-free avatar dot — the agent's identity is its colour and title. */
function AssigneeDot({ assignee }: { assignee: string }) {
  return (
    <span
      title={assignee}
      className="w-4 h-4 rounded-full border-2 border-black bg-brand shrink-0"
    />
  );
}

function IssueCard({ issue, overlay = false }: { issue: Issue; overlay?: boolean }) {
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
      </div>
      <p
        className={`font-mono text-[0.76rem] font-bold leading-snug line-clamp-2 ${
          cancelled ? 'line-through' : ''
        }`}
      >
        {issue.title}
      </p>
      {issue.blocked_reason != null ? (
        <span className="self-start border border-warn/50 bg-warn/10 text-warn rounded px-1.5 font-mono text-[0.56rem] font-bold uppercase">
          ⚑ Blocked
        </span>
      ) : null}
      {issue.assignee != null ? (
        <div className="flex items-center gap-1.5">
          <AssigneeDot assignee={issue.assignee} />
          <span className="font-mono text-[0.58rem] text-ink-soft truncate">{issue.assignee}</span>
        </div>
      ) : null}
    </article>
  );
}
