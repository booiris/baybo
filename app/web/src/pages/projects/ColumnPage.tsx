import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  Link,
  Navigate,
  useLocation,
  useNavigate,
  useParams,
  useSearchParams,
} from 'react-router-dom';
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
  type Agent,
  type Board,
  type Issue,
  type IssueRun,
  type IssueStatus,
  type Project,
  type ReadingBand,
  type StageTally,
  assignableAgents,
  columnHasNews,
  dropRejection,
  emptyBoard,
  findIssue,
  groupByStatus,
  hasDeliverable,
  readingBands,
  readingOrder,
  liveCount,
  moveAnnouncement,
  moveCard,
  persistedOrder,
  runIndicator,
  stageTally,
  statusOf,
  updatedAgo,
  withPin,
  withPositions,
} from './boardModel';
import { boardFilterParams, filterBoard, parseBoardFilter } from './boardFilter';
import type { BoardFilter } from './boardFilter';
import { BoardFilterMenu } from './BoardFilterMenu';
import { OverCeilingChip } from './OverCeilingChip';
import { useTeamPortraits, type Portrait } from './portrait';
import { writeLastProjectId } from './lastProject';
import { CreateIssueModal } from './CreateIssueModal';
import { AgentProfile } from './AgentProfile';
import { FloatingPanel } from './FloatingPanel';
import { Picker } from './Picker';
import { PRIORITY_LABEL, statusChipShape, statusOptions } from './issueFields';
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
import { handleOf } from './teamModel';
import { invalidateAttention } from './useAttention';
import { ToastStack, useToasts } from './Toasts';
import { useBoardStream } from './useBoardStream';
import { useBoardActivity } from './useBoardActivity';

/// One stage of the board, as a whole page — the board's 210px lane opened
/// out to the screen it was maximized onto.
///
/// The board shows a stage as a tall thin queue of cramped tiles; this shows
/// the same stage as a **wall of cards** that uses the width, grouped under
/// the reading order's own three bands. Every fact the board's tile carries
/// is here, drawn bigger: nothing is truncated into a tooltip because there
/// is finally room for it.
///
/// **It does not reorder.** The board is where a stage's order is dragged
/// into shape; here the same cards are read, triaged and handed on — the
/// pin, the Move chip and the card itself. That is a deliberate split, and
/// it is what lets this page lay its cards out in a grid at all: `position`
/// is a one-dimensional rank, and a grid cannot show one without lying about
/// which of two side-by-side cards comes first.
///
/// It stays the board in every other rule: the reading order is never
/// written, a Move sends `persistedOrder`, and the filter is the same URL
/// vocabulary — so a narrowing survives the zoom in and back out.
export function ColumnPage() {
  const { pid, status: statusParam } = useParams<{ pid: string; status: string }>();
  const projectId = pid ?? '';
  const status: IssueStatus | null = (COLUMNS as readonly string[]).includes(statusParam ?? '')
    ? (statusParam as IssueStatus)
    : null;
  const client = useAdminClient();
  const { logout } = useAuth();
  const navigate = useNavigate();
  const location = useLocation();
  const { toasts, pushToast, dismissToast } = useToasts();

  const [project, setProject] = useState<Project | null>(null);
  const [team, setTeam] = useState<Agent[]>([]);
  const portrait = useTeamPortraits(team);
  const [activeRuns, setActiveRuns] = useState<IssueRun[]>([]);
  const [board, setBoard] = useState<Board>(emptyBoard);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);
  const [createOpen, setCreateOpen] = useState(false);
  const [profileOf, setProfileOf] = useState<string | null>(null);
  const [params, setParams] = useSearchParams();
  const filter = useMemo(() => parseBoardFilter(params), [params]);

  const stageBar = useRef<HTMLDivElement | null>(null);

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
      // Applied to the board the fetch produced, not to the rendered view:
      // `readingBands` groups a column it is promised is already in reading
      // order, and one applied a layer up would be a second order.
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

  // Answered the moment it lands. Nothing has to be held back: with no drag
  // there is no gesture in flight for a re-keyed list to resolve against —
  // the board page holds its frames for exactly that reason and this page
  // no longer needs to.
  useBoardStream(projectId, null, refetch);

  // Below `sm` the stage bar is a scroller, and a scroller mounts at the
  // left. Arriving on a late stage — Done starts ~370px into a ~300px strip
  // on a phone — showed four stages the operator is not on and no gold
  // segment at all. A no-op at `sm` and up, where the bar is a five-column
  // grid that does not scroll.
  //
  // **Once per stage**, tracked in a ref rather than by the dep list. It has
  // to keep `loading` as a dep, because the first commit renders the spinner
  // and the bar is not in the DOM yet — but `loading` flips on every refetch,
  // and with the WS hold gone that is every `project_changed` frame. Left to
  // the deps alone, an agent's comment on some other card would yank the bar
  // back from wherever the operator had just swiped it.
  const centred = useRef<IssueStatus | null>(null);
  useEffect(() => {
    if (centred.current === status) return;
    const bar = stageBar.current;
    const here = bar?.querySelector<HTMLElement>('[aria-current="page"]');
    if (bar == null || here == null) return;
    bar.scrollLeft = here.offsetLeft - bar.offsetLeft - (bar.clientWidth - here.offsetWidth) / 2;
    centred.current = status;
  }, [status, loading]);

  const view = useMemo(() => filterBoard(board, filter, team, activeRuns), [activeRuns, board, filter, team]);
  // Same numbers the board page's notice reads: this page is that board.
  const activity = useBoardActivity(true, refreshKey);
  const list = useMemo(() => (status === null ? [] : view[status]), [status, view]);
  const bands = useMemo(
    () => (status === null ? [] : readingBands(list, status)),
    [list, status],
  );
  // A band header only earns its line when there is another band to be told
  // apart from — one header over the whole wall labels nothing.
  const banded = useMemo(() => bands.filter((band) => band.issues.length > 0).length > 1, [bands]);
  const shown = useMemo(() => bands.reduce((total, band) => total + band.issues.length, 0), [bands]);

  type Commit = (before: Board, next: Board, number: number) => Promise<void>;
  // The failed-move toast's Retry re-enters the commit with the same
  // arguments; a ref reaches the current one without the callback having to
  // depend on itself.
  const commitRef = useRef<Commit | null>(null);
  const commitMove: Commit = useCallback(
    async (before, next, number) => {
      // Re-read, exactly as `withPin` does. `readingBands` promises to be a
      // grouping of a column already in reading order, and an optimistic
      // move breaks that precondition the instant it lands — the bands would
      // become a second sort over a column they claim only to group.
      setBoard(readingOrder(next));
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
      // What the destination stores, not what this page shows: the reading
      // order is a reading order, and a move must not write it. `null` for
      // the anchor puts the card at the end of the stage it is joining,
      // exactly as a drop on a column's body does on the board.
      const ordered = persistedOrder(next[moved.status], number, null);
      setBoard(readingOrder(withPositions(next, moved.status, ordered)));
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
            void commitRef.current?.(before, next, number);
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

  const relocate = useCallback(
    (issue: Issue, to: IssueStatus) => {
      if (to === issue.status) return;
      // The one thing a single-stage page cannot do by moving a card around
      // on it. The card joins the end of the stage it is sent to, exactly as
      // a drop on a column's body does on the board.
      void commitMove(board, moveCard(board, { status: to, before: null, issue }), issue.number);
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

  // Where a card opened from here goes back to. The detail page's own link
  // is fixed on the board, which from a maximized stage is two steps from
  // where the operator was — and drops this stage's filter on the way, so
  // the wall they came back to is not the wall they left.
  const here = `${location.pathname}${location.search}`;

  const openIssue = useCallback(
    (number: number) => {
      navigate(`/projects/${encodeURIComponent(projectId)}/issues/${number}`, {
        state: { from: here },
      });
    },
    [here, navigate, projectId],
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
  const tally = stageTally(board[status], activeRuns);
  const matched = liveCount(view[status]);

  return (
    <div className="flex flex-col h-full min-h-0">
      <header className="h-12 shrink-0 px-5 border-b-2 border-black flex items-center gap-3 bg-canvas">
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
        <div className="ml-auto flex items-center gap-2">
          {project === null ? null : (
            <OverCeilingChip
              project={project}
              activity={activity}
              held={activeRuns.filter((run) => run.status === 'held').length}
            />
          )}
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

      {/* The page says what it is before it says what is in it. On the board
          a column is a 210px queue whose name is a 10px label; opened to the
          full width it is the subject of the page, and the operator arrives
          here from four other stages that look exactly the same. */}
      <div className="shrink-0 border-b-2 border-black bg-canvas">
        <div className="px-5 pt-5 pb-4 flex items-end justify-between gap-6">
          <div className="min-w-0 flex flex-col gap-1.5">
            <span className="font-mono text-[0.6rem] font-bold uppercase tracking-[0.2em] text-ink-soft truncate">
              {project?.name ?? 'Project'} · Stage {COLUMNS.indexOf(status) + 1} of {COLUMNS.length}
            </span>
            <h1 className="font-mono text-[1.5rem] sm:text-[2rem] leading-none font-bold uppercase tracking-tight">
              {COLUMN_LABEL[status]}
              {/* Which board's Backlog. On screen the eyebrow above says it,
                  but a reader jumping by heading lands here with nothing
                  before it. */}
              <span className="sr-only"> — {project?.name ?? 'project'}</span>
            </h1>
            <StageStats tally={tally} />
          </div>
          {/* The stage's own size — how much work the stage holds, never the
              filtered count, since a number that shrank when a filter hid
              cards would report an emptied stage. The tabs carry the
              `matched/whole` split.

              Sized **under** the heading it sits beside, not level with it.
              At the `<h1>`'s own 2rem, inside a 72px block with a 3px border
              and a full `shadow-brutal`, the count was the loudest thing on
              the page — a supporting figure outshouting the name of the
              thing it counts. It also grows with its digits rather than
              sitting in a fixed square, so a stage holding 120 cards does
              not clip. */}
          <span
            title={
              matched === tally.live
                ? `${tally.live} live cards in ${COLUMN_LABEL[status]}`
                : `${tally.live} live cards in ${COLUMN_LABEL[status]} — ${matched} match the filter`
            }
            className="shrink-0 min-w-10 h-10 px-2 grid place-items-center border-2 border-black rounded-md bg-brand shadow-brutal-sm font-mono text-[1.05rem] font-bold tabular-nums"
          >
            {tally.live}
          </span>
        </div>

        {/* One instrument rather than five loose pills: the stages are a
            pipeline, and a segmented control is what a pipeline looks like
            when every part of it is one press away. */}
        <nav aria-label="Stages" className="px-5 pb-5">
          {/* Five equal segments where there is room for five. On a phone
              there is not — squeezed to 68px each, every label truncates to
              "IN P…" — so below `sm` it scrolls sideways at its natural
              width instead, with the next stage showing at the edge to say
              so. `overflow-x-auto` clips as well as scrolls, which is what
              keeps the segments inside the rounded border either way. */}
          <div
            ref={stageBar}
            className="flex overflow-x-auto sm:grid sm:grid-cols-5 sm:overflow-hidden border-2 border-black rounded-md shadow-brutal-sm"
          >
            {COLUMNS.map((tab) => {
              const live = liveCount(view[tab]);
              const whole = liveCount(board[tab]);
              const here = tab === status;
              return (
                <Link
                  key={tab}
                  to={{
                    pathname: `/projects/${encodeURIComponent(projectId)}/board/${tab}`,
                    search: params.toString(),
                  }}
                  aria-current={here ? 'page' : undefined}
                  className={`shrink-0 sm:shrink flex items-center justify-between gap-2 px-3 py-2 border-r-2 border-black last:border-r-0 font-mono text-[0.62rem] font-bold uppercase tracking-wider transition-colors ${
                    here ? 'bg-brand text-ink' : 'bg-surface text-ink-soft hover:bg-brand/25'
                  }`}
                >
                  <span className="truncate">{COLUMN_PILL_LABEL[tab]}</span>
                  <span className="flex shrink-0 items-center gap-1.5">
                    {/* A dot, not a number: pressing a tab cannot discharge
                        what it shows — opening cards does. The stage on
                        screen wears none, because its rows carry the counts
                        themselves. */}
                    {!here && columnHasNews(board[tab]) ? (
                      <span
                        title={`Something new in ${COLUMN_LABEL[tab]}`}
                        className="w-2 h-2 rounded-full border border-black bg-err"
                      />
                    ) : null}
                    <span
                      className="rounded-full bg-ink text-brand text-[0.56rem] px-1.5 leading-[1.05rem] tabular-nums"
                      title={
                        live === whole
                          ? `${whole} live`
                          : `${live} of ${whole} live cards match the filter`
                      }
                    >
                      {live === whole ? whole : `${live}/${whole}`}
                    </span>
                  </span>
                </Link>
              );
            })}
          </div>
        </nav>
      </div>

      {error != null ? (
        <div className="m-4 bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm break-words">
          {error}
        </div>
      ) : null}

      {/* Positioning context for the profile panel: an absolute box inside
          the scroller would be placed against the content and scroll away
          with it. */}
      <div className="relative flex-1 min-h-0 flex flex-col">
        <div className="flex-1 min-h-0 overflow-y-auto bg-canvas">
          {/* No reading-width cap. This page is what the maximize button
              opens, and a column frozen at 1152px under a stage bar that
              spans the screen is the page disagreeing with itself — at
              2560 the bar was 2.2× the width of the cards beneath it, with
              the count block stranded 728px past their right edge. */}
          <div className="w-full px-5 py-5 flex flex-col gap-5">
            {shown > 0 ? (
              /* **One** grid, with the band headers spanning it, rather than
                 a grid per band. Three grids means three parents, and React
                 unmounts a card that moves between them — so pinning a card,
                 or a WS refetch that clears its unread, destroyed and rebuilt
                 the very card being interacted with, closing any picker open
                 on it and dropping focus to the body.

                 One more column each time a card would otherwise get wider
                 than it has anything to say. It stops at four: a fifth needs
                 a breakpoint past `2xl`, and an arbitrary `min-[2100px]:` one
                 is emitted *before* the named scale in this Tailwind, so it
                 loses to `2xl:grid-cols-4` at every width matching both — a
                 class that reads as live and does nothing. A fifth column
                 wants a real `--breakpoint-3xl` token first. */
              <div className="grid gap-3 grid-cols-1 md:grid-cols-2 xl:grid-cols-3 2xl:grid-cols-4">
                {bands.flatMap((band) =>
                  band.issues.length === 0
                    ? []
                    : [
                        ...(banded
                          ? [
                              <div key={`band-${band.key}`} className="col-span-full">
                                <BandHeader band={band} />
                              </div>,
                            ]
                          : []),
                        ...band.issues.map((issue) => (
                          <IssueTile
                            key={issue.number}
                            issue={issue}
                            to={`/projects/${encodeURIComponent(projectId)}/issues/${issue.number}`}
                            from={here}
                            run={runIndicator(activeRuns, issue.number)}
                            team={team}
                            portrait={portrait}
                            readOnly={archived}
                            onOpen={openIssue}
                            onOpenAgent={openAgent}
                            onRelocate={relocate}
                            onTogglePin={togglePin}
                          />
                        )),
                      ],
                )}
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

/// What the stage is doing, under its own name — and only when it is doing
/// it. A heading that always prints `0 working · 0 unread · 0 failed` reads
/// as furniture and stops being looked at; these three lines exist to be
/// noticed, so a quiet stage says nothing at all.
///
/// The row keeps its height either way: five stages that differ by a line
/// would move the whole list up and down as the operator walks the tabs.
function StageStats({ tally }: { tally: StageTally }) {
  const quiet = tally.working === 0 && tally.unread === 0 && tally.failed === 0;
  return (
    <div className="flex min-h-[1.05rem] items-center gap-3 font-mono text-[0.66rem] tabular-nums">
      {quiet ? (
        <span className="text-ink-soft">Nothing waiting on you</span>
      ) : (
        <>
          {tally.working > 0 ? (
            <span className="text-ink-soft" title="Runs in flight on this stage">
              <b className="text-ok">{tally.working}</b> working
            </span>
          ) : null}
          {tally.unread > 0 ? (
            <span className="text-ink-soft" title="Cards with something new since you opened them">
              <b className="text-err">{tally.unread}</b> new
            </span>
          ) : null}
          {tally.failed > 0 ? (
            <span className="text-ink-soft" title="Cards whose newest run failed">
              <b className="text-err">{tally.failed}</b> run failed
            </span>
          ) : null}
        </>
      )}
    </div>
  );
}

/// The rule the order follows, written on the page it orders.
///
/// A strip rather than a heading: it separates two runs of rows inside one
/// surface, and a card-like block per band would break the list into four
/// lists — see the fragment in `ColumnPage`.
function BandHeader({ band }: { band: ReadingBand }) {
  return (
    <div
      title={band.note}
      className="flex items-center gap-2.5 font-mono text-[0.62rem] font-bold uppercase tracking-[0.18em] text-ink-soft"
    >
      {/* A heading, so a reader can jump band to band the way the eye does
          down the labels. */}
      <h2 className="font-bold text-ink">{band.label}</h2>
      <span className="rounded-full bg-ink text-brand text-[0.56rem] px-1.5 leading-[1.05rem] tabular-nums">
        {band.issues.length}
      </span>
      {band.key === 'news' ? (
        <span aria-hidden className="w-2 h-2 rounded-full border border-black bg-err" />
      ) : null}
      {/* The rule runs the width of the grid it introduces, which is what
          makes three stacked grids read as three sections of one wall
          rather than as three unrelated walls. */}
      <span aria-hidden className="flex-1 h-0.5 bg-ink/25" />
    </div>
  );
}

/// One issue as one card.
///
/// This is the board's tile with the room it never had. On a 210px lane a
/// tile has to choose: the title wraps to two cramped lines and the branch,
/// the run word and the assignee fight over one footer. Here a card is
/// 400-600px, so the same facts sit in three honest zones — a header line of
/// identity, the title, and a footer of who and what next — and the title
/// gets two full lines before it is ever cut.
///
/// The card is a press: it opens the issue. Everything inside it that does
/// something else — the pin, the assignee's face, the branch chip, the Move
/// picker — claims its own press, or the operator could never reach it.
function IssueTile({
  issue,
  to,
  from,
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
  /// Where the title's link goes. The card's own press navigates too, but a
  /// press is not a door: a link is what a keyboard reaches, what a screen
  /// reader lists, and what a middle-click opens in a tab.
  to: string;
  /// This page, so the card's own page can come back to it rather than to
  /// the board. Carried in router state, which survives a reload and is
  /// absent on a pasted URL — where the board is the honest answer.
  from: string;
  run: AvatarRun;
  team: Agent[];
  portrait: Portrait;
  readOnly: boolean;
  onOpen: (number: number) => void;
  onOpenAgent: (agentId: string) => void;
  onRelocate: (issue: Issue, to: IssueStatus) => void;
  onTogglePin: (number: number, pinned: boolean) => void;
}) {
  const cancelled = issue.cancelled_at_ms != null;
  const priority = PRIORITY_MARK[issue.priority];
  const marks =
    issue.blocked_reason != null || issue.last_run_failed || (hasDeliverable(issue) && issue.branch != null);
  return (
    <article
      // Two things this card must not do, both of which cost the Move
      // picker its panel. It is **not `overflow-hidden`** — the panel is
      // absolutely positioned inside it, and a clipping ancestor eats it
      // (the spine is inset rather than clipped instead). And it does
      // **not transform on hover**: a non-`none` `translate` makes the card
      // a stacking context, which confines the panel's `z-30` to it while
      // every later card in the grid paints on top. Measured, only 13% of
      // the panel stayed hit-testable, and because a press then landed on
      // two different elements the `click` fired on their common ancestor —
      // so the option did nothing at all. The shadow alone is the hover.
      className={`group relative flex flex-col border-2 border-black rounded-md bg-surface shadow-brutal-sm cursor-pointer transition-[box-shadow] hover:shadow-brutal ${
        cancelled ? 'opacity-55' : ''
      }`}
      onClick={() => {
        onOpen(issue.number);
      }}
    >
      {/* Priority down the card's left edge. On the row it was the one edge
          you swept; on a wall it is what lets a column of cards still be
          read for urgency without reading a word of any of them. Inset from
          the ends so it clears the corner radius the card no longer clips. */}
      {priority === null ? null : (
        <span aria-hidden className={`absolute left-0 inset-y-2 w-[3px] ${priority.spine}`} />
      )}

      <div className="flex items-center gap-2 pl-4 pr-2.5 pt-2 font-mono text-[0.6rem] text-ink-soft">
        <PinButton
          pinned={issue.pinned}
          disabled={readOnly}
          onToggle={(pinned) => {
            onTogglePin(issue.number, pinned);
          }}
        />
        <span className="font-bold tabular-nums">#{issue.number}</span>
        {priority === null ? null : (
          <span className={`truncate ${priority.tone}`}>
            {priority.glyph} {PRIORITY_LABEL[issue.priority]}
          </span>
        )}
        <span
          className="ml-auto shrink-0 tabular-nums"
          title={`Last touched ${new Date(issue.updated_at_ms).toLocaleString()}`}
        >
          {updatedAgo(issue.updated_at_ms, Date.now())}
        </span>
        {issue.unread > 0 ? <UnreadPill unread={issue.unread} /> : null}
      </div>

      {/* Two lines before it truncates. The row could afford one; a card is
          the reason the grid does not cost a readable title.

          The title is a **link**, and it is the card's only keyboard door.
          The card's own press is a mouse convenience; dropping dnd-kit took
          its `attributes` with it, and those were what had been quietly
          supplying the tab stop. A link is also what a screen reader lists
          and what a middle-click opens in a tab. It stops the press from
          bubbling so the card's `onClick` does not navigate a second time —
          `preventDefault` would be wrong here, since the navigation is the
          link's own job. */}
      <p
        className={`pl-4 pr-2.5 pt-1.5 font-mono text-[0.82rem] font-bold leading-snug line-clamp-2 ${
          cancelled ? 'line-through' : ''
        }`}
        title={issue.title}
      >
        <Link
          to={to}
          state={{ from }}
          className="outline-none focus-visible:underline focus-visible:decoration-2 focus-visible:underline-offset-2"
          onClick={(event) => {
            event.stopPropagation();
          }}
        >
          {issue.title}
        </Link>
      </p>

      {marks ? (
        <div className="flex flex-wrap items-center gap-1.5 pl-4 pr-2.5 pt-2">
          {issue.blocked_reason != null ? <BlockedBadge reason={issue.blocked_reason} /> : null}
          {issue.last_run_failed ? <FailedBadge /> : null}
          {hasDeliverable(issue) && issue.branch != null ? (
            <BranchChip branch={issue.branch} />
          ) : null}
        </div>
      ) : null}

      {/* Pushed to the card's foot, so a short title and a long one give the
          same card: a grid row is as tall as its tallest card, and footers
          that floated would leave every short card with a hole in it. */}
      <div className="mt-auto flex items-center gap-2 pl-4 pr-2.5 pb-2 pt-3">
        <span className="flex min-w-0 flex-1 items-center gap-1.5">
          {issue.assignee != null ? (
            <AssigneeFace
              handle={handleOf(team, issue.assignee)}
              src={portrait(issue.assignee)}
              run={run}
              onOpen={() => {
                onOpenAgent(issue.assignee ?? '');
              }}
            />
          ) : (
            <UnassignedMark />
          )}
        </span>
        {issue.sub_issues != null ? <SubIssueRing progress={issue.sub_issues} /> : null}
        <RunWord run={run} />
        {/* The stage this card is in is the page's own heading, so the chip
            wears the action instead of the value. Its press must not fall
            through to the card, which opens the issue. */}
        {readOnly ? null : (
          <span
            className="shrink-0"
            onClick={(event) => {
              event.preventDefault();
              event.stopPropagation();
            }}
          >
            <Picker
              label={`Move #${issue.number}`}
              // The stage is state, not the object of the verb: the default
              // name would read "Move #3: Review" on a card already in
              // Review — naming the one destination the picker refuses.
              ariaLabel={`Move #${issue.number} — currently in ${COLUMN_LABEL[issue.status]}`}
              title="Move this card to another stage"
              value={issue.status}
              disabled={false}
              options={statusOptions}
              onPick={(picked) => {
                onRelocate(issue, picked as IssueStatus);
              }}
              triggerClassName={`${statusChipShape} pl-2 pr-0.5 border-black/25 text-ink-soft transition-colors group-hover:border-black group-hover:bg-canvas group-hover:text-ink focus-visible:border-black focus-visible:bg-canvas focus-visible:text-ink`}
            >
              Move
            </Picker>
          </span>
        )}
      </div>
    </article>
  );
}
