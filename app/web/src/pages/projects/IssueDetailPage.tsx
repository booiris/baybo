import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from 'react';
import { Link, useParams } from 'react-router-dom';
import { RiArrowLeftLine, RiLoader4Line } from 'react-icons/ri';

import { useAdminClient, useAuth } from '../../api/auth';
import {
  cancelRun,
  fetchTeam,
  resolveApproval,
  fetchIssue,
  fetchIssueRuns,
  fetchIssues,
  fetchTimeline,
  moveIssue,
  patchIssue,
  postComment,
} from './api';
import {
  COLUMNS,
  COLUMN_LABEL,
  PRIORITIES,
  assignableAgents,
  runDuration,
  unsettledRun,
  type Agent,
  type Issue,
  type IssueRun,
  type RunLog,
  type IssuePriority,
  type IssueStatus,
} from './boardModel';
import { MarkdownBody } from '../ChatPage';
import { Avatar } from './Avatar';
import { PickerOverlay } from './PickerOverlay';
import { SubIssues } from './SubIssues';
import { Timeline } from './Timeline';
import type { IssueEvent } from './timelineModel';
import { useBoardStream } from './useBoardStream';
import { formatTokens, formatUsd } from './budgetModel';
import { handleOf } from './teamModel';

const PRIORITY_LABEL: Record<IssuePriority, string> = {
  urgent: 'Urgent',
  high: 'High',
  medium: 'Medium',
  low: 'Low',
  none: 'No priority',
};

/// Priority is read off this rail at a glance or not at all, so it carries
/// its own colour rather than sitting in the same ink as everything else.
const PRIORITY_TONE: Record<IssuePriority, string> = {
  urgent: 'text-err',
  high: 'text-warn',
  medium: 'text-ink',
  low: 'text-ink-soft',
  none: 'text-ink-soft',
};

const railLabel = 'font-mono text-[0.6rem] font-bold uppercase tracking-[0.12em] text-ink-soft';

const railBox = 'border-2 border-black rounded-md bg-surface px-3 py-2.5';

/// One `label ── value` line of the property table.
function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex items-baseline justify-between gap-2.5 py-[3px] font-mono text-[0.68rem]">
      <span className="shrink-0 text-ink-soft">{label}</span>
      <span className="min-w-0 text-right font-bold break-words">{children}</span>
    </div>
  );
}

const RUN_TONE: Record<IssueRun['status'], string> = {
  held: 'border-warn/50 bg-warn/12 text-warn',
  queued: 'border-black/35 bg-canvas text-ink-soft',
  running: 'border-black bg-brand/40 text-ink',
  done: 'border-ok/50 bg-ok/15 text-ok',
  failed: 'border-err/45 bg-err/12 text-err',
  cancelled: 'border-black/35 bg-canvas text-ink-soft',
};

/// Why this run happened. The mockup writes the first one "drag", which is
/// only ever one of the three ways a card reaches In Progress — a REST move
/// and an agent's own tool call produce the same trigger, and a log that
/// blames a drag for either is lying about who started the work.
const RUN_TRIGGER_LABEL: Record<IssueRun['trigger'], string> = {
  started: 'moved to In Progress',
  assigned: 'assigned',
  retry: 'retry',
  comment: 'comment',
  promoted: 'the board had room',
  triage: 'nobody assigned',
  stage_barrier: 'stage barrier',
};

function RunRow({
  run,
  onCancel,
  busy,
}: {
  run: IssueRun;
  onCancel: () => void;
  busy: boolean;
}) {
  const duration = runDuration(run, Date.now());
  const live = run.status === 'queued' || run.status === 'running' || run.status === 'held';
  // A live run can be stopped; every run has a transcript once it has a
  // session to open. Nothing here starts one — work begins by moving the
  // card, putting somebody on it, commenting, a stage barrier, or the board
  // taking it off the top of Todo.
  return (
    <li className="flex flex-wrap items-center gap-2 py-1.5 border-b border-black/15 last:border-0 font-mono text-[0.62rem]">
      <span className="font-bold">#{run.attempt}</span>
      <span className="text-ink-soft">{RUN_TRIGGER_LABEL[run.trigger]}</span>
      <span
        className={`rounded-full border px-2 font-bold uppercase tracking-wider ${RUN_TONE[run.status]}`}
      >
        {run.status}
      </span>
      {duration != null ? <span className="text-ink-soft">{duration}</span> : null}
      {run.cost_micros != null && run.cost_micros > 0 ? (
        <span className="text-ink-soft tabular-nums" title="What this run's model calls cost">
          {formatUsd(run.cost_micros)}
        </span>
      ) : null}
      <span className="ml-auto flex items-center gap-2 font-bold">
        {live ? (
          <button
            type="button"
            className="text-err underline cursor-pointer disabled:opacity-50"
            disabled={busy}
            onClick={onCancel}
          >
            Cancel
          </button>
        ) : null}
        {run.session_id != null ? (
          <a className="text-info underline" href={`#/traces/${encodeURIComponent(run.session_id)}`}>
            Transcript
          </a>
        ) : null}
      </span>
    </li>
  );
}

export function IssueDetailPage() {
  const { pid, num } = useParams<{ pid: string; num: string }>();
  const projectId = pid ?? '';
  const number = Number(num ?? '');
  const client = useAdminClient();
  const { logout } = useAuth();

  const [issue, setIssue] = useState<Issue | null>(null);
  const [board, setBoard] = useState<Issue[]>([]);
  const [agents, setAgents] = useState<Agent[]>([]);
  const [runLog, setRunLog] = useState<RunLog | null>(null);
  const runs = runLog?.items ?? [];
  const [events, setEvents] = useState<IssueEvent[]>([]);
  const [refreshKey, setRefreshKey] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [title, setTitle] = useState('');
  const [description, setDescription] = useState('');
  const [saving, setSaving] = useState(false);
  const [showMore, setShowMore] = useState(false);
  const [copied, setCopied] = useState(false);
  const [editingTitle, setEditingTitle] = useState(false);
  const [editingDescription, setEditingDescription] = useState(false);
  const serverProse = useRef({ title: '', description: '' });
  const descriptionBox = useRef<HTMLTextAreaElement | null>(null);
  const pane = useRef<HTMLElement | null>(null);
  /// Whether this card has already been dropped at the foot of its timeline.
  /// Reset per card, so opening a second one lands the same way.
  const landed = useRef(false);

  // The field grows to its text, so entering and leaving the editor does not
  // move the page. A textarea's own height comes from `rows`, which is a
  // count of lines and knows nothing about the rendered markdown standing in
  // its place — a long description would collapse to the minimum the moment
  // it was clicked, and spring back on blur. Laid out before paint, or the
  // jump happens anyway and is merely one frame shorter.
  useLayoutEffect(() => {
    const box = descriptionBox.current;
    if (box === null) return;
    box.style.height = 'auto';
    box.style.height = `${String(box.scrollHeight)}px`;
  }, [description, editingDescription]);

  // A card opens at the foot of its own history — the newest entries and the
  // composer — rather than at a title you have already read. Once per card,
  // and never on a refresh: the timeline invalidates on every board frame,
  // and re-anchoring there would yank the page out from under somebody
  // reading back through it. Before paint, so it lands rather than jumps.
  useEffect(() => {
    landed.current = false;
  }, [projectId, number]);

  useLayoutEffect(() => {
    if (loading || issue === null || landed.current) return;
    const box = pane.current;
    if (box === null) return;
    landed.current = true;
    box.scrollTop = box.scrollHeight;
  }, [loading, issue]);

  useEffect(() => {
    let canceled = false;
    async function load() {
      const [outcome, agentsOutcome, runsOutcome, timelineOutcome, boardOutcome] =
        await Promise.all([
          fetchIssue(client, projectId, number),
          fetchTeam(client, projectId),
          fetchIssueRuns(client, projectId, number),
          fetchTimeline(client, projectId, number),
          fetchIssues(client, projectId),
        ]);
      if (canceled) return;
      if (boardOutcome.kind === 'ok') setBoard(boardOutcome.value);
      if (agentsOutcome.kind === 'ok') setAgents(assignableAgents(agentsOutcome.value));
      if (runsOutcome.kind === 'ok') setRunLog(runsOutcome.value);
      if (timelineOutcome.kind === 'ok') setEvents(timelineOutcome.value);
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setError(outcome.message);
        setLoading(false);
        return;
      }
      setIssue(outcome.value);
      // Live refreshes must not overwrite an edit in progress — so the
      // editors only follow the server while they still hold what the server
      // last sent.
      //
      // The comparison reads a **snapshot** taken before the ref moves. A
      // lazy updater runs during the next render, by which time an assignment
      // further down this function has already advanced `serverProse`, so
      // reading the ref inside the updater compared the old draft against the
      // *new* server value — never equal on a first load, which left both
      // editors empty on every card and armed Save to write those blanks
      // back.
      const previous = serverProse.current;
      serverProse.current = {
        title: outcome.value.title,
        description: outcome.value.description,
      };
      setTitle((current) => (current === previous.title ? outcome.value.title : current));
      setDescription((current) =>
        current === previous.description ? outcome.value.description : current,
      );
      setError(null);
      setLoading(false);
    }
    void load();
    return () => {
      canceled = true;
    };
  }, [client, logout, number, projectId, refreshKey]);

  const bumpRefresh = useCallback(() => {
    setRefreshKey((key) => key + 1);
  }, []);

  useBoardStream(projectId, number, bumpRefresh);

  const apply = useCallback(
    async (body: Parameters<typeof patchIssue>[3]) => {
      setSaving(true);
      const outcome = await patchIssue(client, projectId, number, body);
      setSaving(false);
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setError(outcome.message);
        return;
      }
      setError(null);
      setIssue(outcome.value);
      setTitle(outcome.value.title);
      setDescription(outcome.value.description);
      serverProse.current = {
        title: outcome.value.title,
        description: outcome.value.description,
      };
    },
    [client, logout, number, projectId],
  );

  const moveChild = useCallback(
    async (childNumber: number, status: IssueStatus) => {
      setSaving(true);
      const latest = await fetchIssues(client, projectId);
      if (latest.kind === 'unauthorized') {
        logout();
        setSaving(false);
        return;
      }
      if (latest.kind === 'failed') {
        setError(latest.message);
        setSaving(false);
        return;
      }
      const ordered = latest.value
        .filter((candidate) => candidate.status === status && candidate.number !== childNumber)
        .sort((a, b) => a.position - b.position)
        .map((candidate) => candidate.number);
      ordered.push(childNumber);
      const outcome = await moveIssue(client, projectId, childNumber, status, ordered);
      setSaving(false);
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setError(outcome.message);
        return;
      }
      setError(null);
      setRefreshKey((key) => key + 1);
    },
    [client, logout, projectId],
  );

  const relocate = useCallback(
    async (status: IssueStatus) => {
      setSaving(true);
      const board = await fetchIssues(client, projectId);
      if (board.kind === 'unauthorized') {
        logout();
        setSaving(false);
        return;
      }
      if (board.kind === 'failed') {
        setError(board.message);
        setSaving(false);
        return;
      }
      const ordered = board.value
        .filter((candidate) => candidate.status === status && candidate.number !== number)
        .sort((a, b) => a.position - b.position)
        .map((candidate) => candidate.number);
      ordered.push(number);

      const outcome = await moveIssue(client, projectId, number, status, ordered);
      setSaving(false);
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setError(outcome.message);
        return;
      }
      setError(null);
      setIssue(outcome.value);
    },
    [client, logout, number, projectId],
  );

  const stopRun = useCallback(async () => {
    setSaving(true);
    const outcome = await cancelRun(client, projectId, number);
    setSaving(false);
    if (outcome.kind === 'unauthorized') {
      logout();
      return;
    }
    if (outcome.kind === 'failed') {
      setError(outcome.message);
      return;
    }
    setError(null);
    setRefreshKey((key) => key + 1);
  }, [client, logout, number, projectId]);

  const comment = useCallback(
    async (text: string) => {
      setSaving(true);
      const outcome = await postComment(client, projectId, number, text);
      setSaving(false);
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setError(outcome.message);
        return;
      }
      setError(null);
      setEvents((current) => [...current, outcome.value]);
    },
    [client, logout, number, projectId],
  );

  const answerApproval = useCallback(
    async (callId: string, decision: 'approve' | 'approve_always' | 'deny') => {
      setSaving(true);
      const outcome = await resolveApproval(client, projectId, number, callId, decision);
      setSaving(false);
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setError(outcome.message);
        return;
      }
      setError(null);
      setRefreshKey((key) => key + 1);
    },
    [client, logout, number, projectId],
  );

  /// Put somebody on a step without leaving the parent. Goes through the
  /// same patch the card's own rail uses, so the trigger predicate sees an
  /// assignment identically whichever page made it.
  const assignChild = useCallback(
    async (childNumber: number, assignee: string | null) => {
      setSaving(true);
      const outcome = await patchIssue(client, projectId, childNumber, { assignee });
      setSaving(false);
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setError(outcome.message);
        return;
      }
      setError(null);
      setRefreshKey((key) => key + 1);
    },
    [client, logout, projectId],
  );

  /// Leaving a field is what saves it. With no Save button the write has to
  /// ride the gesture that ends the edit, so blur and Enter commit — and
  /// Escape puts the server's text back and commits nothing, which is the
  /// only way left to change your mind.
  ///
  /// A title erased to nothing is treated as a slip rather than an
  /// instruction: it reverts. Blanking a card's title from a field you were
  /// only passing through is not an edit anybody means to make, and the
  /// board would show the card as `Untitled` with no way to tell that from a
  /// card that never had one.
  const commitTitle = useCallback(
    (next: string) => {
      setEditingTitle(false);
      if (next.trim() === '') {
        setTitle(issue?.title ?? '');
        return;
      }
      if (issue != null && next !== issue.title) void apply({ title: next });
    },
    [apply, issue],
  );

  const commitDescription = useCallback(
    (next: string) => {
      setEditingDescription(false);
      if (issue != null && next !== issue.description) void apply({ description: next });
    },
    [apply, issue],
  );

  /// Copying the branch name, defensively. `navigator.clipboard` is typed as
  /// always present but is **absent** outside a secure context — which a
  /// dashboard served over plain http on a LAN address is — so reading
  /// `.writeText` off `undefined` throws synchronously. The board's branch
  /// chip has guarded this since it shipped; this rail did not.
  const copyBranch = useCallback((branch: string | null | undefined) => {
    if (branch == null) return;
    try {
      void navigator.clipboard.writeText(branch).then(
        () => {
          setCopied(true);
          window.setTimeout(() => {
            setCopied(false);
          }, 1200);
        },
        () => {
          // Nothing: the name is still in the title attribute, and a
          // "copied" that did not happen is worse than silence.
        },
      );
    } catch {
      // Same reasoning.
    }
  }, []);

  if (loading) {
    return (
      <div className="flex flex-1 items-center justify-center">
        <RiLoader4Line className="text-3xl text-ink-soft animate-spin" />
      </div>
    );
  }

  if (issue === null) {
    return (
      <div className="p-6">
        <div className="bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm break-words">
          {error ?? 'Issue not found'}
        </div>
      </div>
    );
  }

  const cancelled = issue.cancelled_at_ms != null;
  const blocked = issue.blocked_reason != null;
  const live = unsettledRun(runs);
  const children = board.filter((candidate) => candidate.parent === issue.number);

  return (
    <div className="flex flex-col h-full min-h-0">
      <header className="h-12 shrink-0 px-4 border-b-2 border-black flex items-center gap-3 bg-canvas">
        <Link
          to={`/projects/${encodeURIComponent(projectId)}`}
          className="inline-flex items-center gap-1 font-mono text-[0.72rem] text-ink-soft hover:text-ink"
        >
          <RiArrowLeftLine /> Board
        </Link>
        <span className="font-mono text-[0.85rem] font-bold">#{issue.number}</span>
        <span className="border-2 border-black bg-brand/35 rounded px-2 font-mono text-[0.6rem] font-bold uppercase tracking-wider">
          {COLUMN_LABEL[issue.status]}
        </span>
        {cancelled ? (
          <span className="border-2 border-black bg-canvas rounded px-2 font-mono text-[0.6rem] font-bold uppercase tracking-wider text-ink-soft">
            Cancelled
          </span>
        ) : null}
        {live !== null && issue.assignee != null ? (
          // The same run-status frame the board card and the team strip
          // read, so the shimmer means one thing everywhere.
          <span
            className="ml-auto flex items-center gap-2 font-mono text-[0.66rem] text-ink-soft"
            title={`run #${live.attempt} · ${RUN_TRIGGER_LABEL[live.trigger]} · ${live.status}`}
          >
            <Avatar
              handle={handleOf(agents, issue.assignee)}
              run={live.status === 'running' ? 'running' : live.status === 'held' ? 'held' : 'queued'}
              size="lg"
            />
            @{handleOf(agents, issue.assignee)}{' '}
            {live.status === 'running'
              ? 'is working'
              : live.status === 'held'
                ? 'is held on budget'
                : 'is queued'}{' '}
            · run #{live.attempt}
          </span>
        ) : null}
      </header>

      <div className="flex-1 min-h-0 flex overflow-hidden bg-surface">
        <main
          ref={pane}
          className="flex-1 min-w-0 overflow-y-auto overscroll-none py-5 pl-18 pr-5"
        >
          {error != null ? (
            <div className="mb-4 bg-white border-2 border-err text-err rounded-md px-3 py-2 font-mono text-[0.78rem] break-words">
              {error}
            </div>
          ) : null}

          {/* A heading that becomes a field, not a field wearing a heading's
              clothes. The title sat in a permanently-live borderless input,
              which read as a nameless text box stacked on the description's
              nameless text box — a two-field form where the mockup has a
              document. Both halves now share one read-then-edit gesture, and
              leaving the field is what writes it. */}
          <h1 className="font-mono text-[1.15rem] font-bold leading-[1.7rem]">
            {editingTitle ? (
              <input
                className="block w-full bg-transparent border-b-2 border-black outline-none font-mono text-[1.15rem] font-bold leading-[1.7rem]"
                value={title}
                autoFocus
                aria-label="Issue title"
                onBlur={() => {
                  commitTitle(title);
                }}
                onKeyDown={(event) => {
                  if (event.key === 'Enter') commitTitle(title);
                  if (event.key === 'Escape') {
                    setTitle(issue.title);
                    setEditingTitle(false);
                  }
                }}
                onChange={(event) => {
                  setTitle(event.target.value);
                }}
              />
            ) : (
              <button
                type="button"
                title="Click to edit"
                onClick={() => {
                  setEditingTitle(true);
                }}
                className={`block w-full text-left border-b-2 border-transparent leading-[1.7rem] cursor-text underline-offset-4 hover:underline hover:decoration-dashed hover:decoration-black/40 ${
                  cancelled ? 'line-through text-ink-soft' : ''
                }`}
              >
                {title.trim() === '' ? (
                  <span className="font-normal text-ink-soft">Untitled</span>
                ) : (
                  title
                )}
              </button>
            )}
          </h1>
          {/* The mode is the edit flag and nothing else. It used to also open
              the field whenever the text was empty — so on a card with no
              description you got a textarea you had not asked for, and the
              first character you typed made the text non-empty, flipped the
              condition, and swapped the field out from under the cursor.
              One character in, and typing stopped. */}
          {editingDescription ? (
            <textarea
              ref={descriptionBox}
              // Metrically identical to the read view above it, or clicking
              // in resizes the text under the cursor: `.chat-prose` is
              // unlayered on purpose (`index.css:147`) so its 15px/1.7 beats
              // any `text-*` utility, and the field was sitting at 13.6px.
              className="mt-1 block -mx-3 w-[calc(100%+1.5rem)] min-h-24 bg-canvas rounded-md px-3 py-2.5 font-sans text-[15px] leading-relaxed outline-none resize-none overflow-hidden"
              placeholder="What, why, and what done looks like…"
              aria-label="Issue description"
              autoFocus
              onBlur={() => {
                commitDescription(description);
              }}
              onKeyDown={(event) => {
                if (event.key === 'Escape') {
                  setDescription(issue.description);
                  setEditingDescription(false);
                }
              }}
              value={description}
              onChange={(event) => {
                setDescription(event.target.value);
              }}
            />
          ) : (
            // Read mode renders the markdown the description is written in,
            // through the same component the chat thread uses — a raw
            // textarea showed `**bold**` and bare `-` bullets to a reader
            // who only wanted to read it.
            <div
              role="button"
              tabIndex={0}
              title="Click to edit"
              // Pulled left by its own padding, so the prose starts on the
              // same edge as the title and every heading below it. The box
              // keeps the padding — a tinted band with text against its edge
              // reads as clipped — and the bleed goes both ways, staying
              // inside the pane's own padding so nothing scrolls sideways.
              className="mt-1 -mx-3 w-[calc(100%+1.5rem)] min-h-24 rounded-md px-3 py-2.5 chat-prose cursor-text hover:bg-canvas"
              onClick={() => {
                setEditingDescription(true);
              }}
              onKeyDown={(event) => {
                if (event.key === 'Enter') setEditingDescription(true);
              }}
            >
              {description.trim() === '' ? (
                // Same shape the title uses for a card with no name: an
                // invitation you click, not a field lying in wait.
                <p className="text-ink-soft">What, why, and what done looks like…</p>
              ) : (
                <MarkdownBody text={description} />
              )}
            </div>
          )}
          <SubIssues
            projectId={projectId}
            children={children}
            team={agents}
            disabled={saving}
            onStatus={(childNumber, status) => {
              void moveChild(childNumber, status);
            }}
            onAssignee={(childNumber, assignee) => {
              void assignChild(childNumber, assignee);
            }}
          />

          <Timeline
            events={events}
            issue={issue}
            runs={runs}
            busy={saving}
            onComment={(text) => {
              void comment(text);
            }}
            team={agents}
            onResolveApproval={(callId, decision) => {
              void answerApproval(callId, decision);
            }}
          />
        </main>

        <aside className="w-[340px] shrink-0 border-l-2 border-black bg-canvas p-3.5 flex flex-col gap-3.5 overflow-y-auto overscroll-none">
          <section className={railBox}>
            <div className="flex items-center gap-2">
              <h2 className={railLabel}>Properties</h2>
              <div className="ml-auto relative">
                <button
                  type="button"
                  aria-label="More actions"
                  aria-expanded={showMore}
                  title="Block, cancel…"
                  onClick={() => {
                    setShowMore((value) => !value);
                  }}
                  className="px-1 font-mono text-[0.8rem] font-bold leading-none cursor-pointer text-ink-soft hover:text-ink"
                >
                  ⋯
                </button>
                {showMore ? (
                  <div className="absolute right-0 top-[calc(100%+4px)] z-30 w-[190px] bg-surface border-2 border-black rounded-md shadow-brutal py-1">
                    {blocked ? (
                      <button
                        type="button"
                        className="w-full text-left px-3 py-1.5 font-mono text-[0.7rem] cursor-pointer hover:bg-canvas"
                        onClick={() => {
                          setShowMore(false);
                          void apply({ blocked_reason: null });
                        }}
                      >
                        Unblock
                      </button>
                    ) : (
                      <button
                        type="button"
                        className="w-full text-left px-3 py-1.5 font-mono text-[0.7rem] cursor-pointer hover:bg-canvas"
                        onClick={() => {
                          setShowMore(false);
                          const reason = window.prompt('What is this blocked on?');
                          if (reason != null && reason.trim().length > 0) {
                            void apply({ blocked_reason: reason });
                          }
                        }}
                      >
                        Block…
                      </button>
                    )}
                    <button
                      type="button"
                      className={`w-full text-left px-3 py-1.5 font-mono text-[0.7rem] cursor-pointer hover:bg-canvas ${
                        cancelled ? '' : 'text-err'
                      }`}
                      onClick={() => {
                        setShowMore(false);
                        void apply({ cancelled: !cancelled });
                      }}
                    >
                      {cancelled ? 'Reopen issue' : 'Cancel issue'}
                    </button>
                    <p className="px-3 pt-1 font-mono text-[0.55rem] text-ink-soft leading-snug">
                      Cancelling keeps the issue, its number and its history — it just stops
                      counting as live work.
                    </p>
                  </div>
                ) : null}
              </div>
            </div>
            <div className="mt-1.5">
              <PickerOverlay
                label="Status"
                className="flex w-full"
                value={issue.status}
                disabled={saving}
                options={COLUMNS.map((status) => ({ value: status, label: COLUMN_LABEL[status] }))}
                onPick={(picked) => {
                  void relocate(picked as IssueStatus);
                }}
              >
                <Row label="Status">{COLUMN_LABEL[issue.status]}</Row>
              </PickerOverlay>
              <PickerOverlay
                label="Priority"
                className="flex w-full"
                value={issue.priority}
                disabled={saving}
                options={PRIORITIES.map((priority) => ({
                  value: priority,
                  label: PRIORITY_LABEL[priority],
                }))}
                onPick={(picked) => {
                  void apply({ priority: picked as IssuePriority });
                }}
              >
                <Row label="Priority">
                  <span className={PRIORITY_TONE[issue.priority]}>
                    {PRIORITY_LABEL[issue.priority]}
                  </span>
                </Row>
              </PickerOverlay>
              <PickerOverlay
                label="Assignee"
                className="flex w-full"
                value={issue.assignee ?? ''}
                disabled={saving}
                options={[
                  { value: '', label: 'Unassigned' },
                  ...agents.map((agent) => ({
                    value: agent.id,
                    label: `@${agent.handle} — ${agent.name}`,
                  })),
                ]}
                onPick={(picked) => {
                  void apply({ assignee: picked.length > 0 ? picked : null });
                }}
              >
                <Row label="Assignee">
                  {issue.assignee == null ? (
                    <span className="font-normal text-ink-soft">Unassigned</span>
                  ) : (
                    <span className="inline-flex items-center gap-1.5">
                      <Avatar handle={handleOf(agents, issue.assignee)} size="sm" />@
                      {handleOf(agents, issue.assignee)}
                    </span>
                  )}
                </Row>
              </PickerOverlay>
              <Row label="Parent">
                {issue.parent == null ? (
                  <span className="font-normal text-ink-soft">—</span>
                ) : (
                  <Link
                    to={`/projects/${encodeURIComponent(projectId)}/issues/${String(issue.parent)}`}
                    className="underline"
                  >
                    #{issue.parent} · stage {issue.stage}
                  </Link>
                )}
              </Row>
              <Row label="Blocked">
                {blocked ? (
                  <span className="text-warn">⚑ {issue.blocked_reason ?? ''}</span>
                ) : (
                  <span className="font-normal text-ink-soft">—</span>
                )}
              </Row>
            </div>
          </section>

          {issue.branch != null ? (
            <section className={railBox}>
              <h2 className={railLabel}>Branch</h2>
              <div className="mt-1.5 flex items-center gap-2 font-mono text-[0.66rem]">
                <span aria-hidden className="shrink-0 text-ink-soft">
                  ⎇
                </span>
                <code className="min-w-0 flex-1 truncate" title={issue.branch}>
                  {issue.branch}
                </code>
                <button
                  type="button"
                  className="shrink-0 font-bold text-info underline cursor-pointer"
                  onClick={() => {
                    copyBranch(issue.branch);
                  }}
                >
                  {copied ? 'copied' : 'copy'}
                </button>
              </div>
              <p className="mt-1.5 font-mono text-[0.56rem] text-ink-soft leading-snug">
                No diff and no merge button here — take the branch locally, or ask the assignee in a
                comment. The worktree is reclaimed once this card reaches Done.
              </p>
            </section>
          ) : null}

          <section className={railBox}>
            <h2 className={railLabel}>Execution log</h2>
            {/* Read-only, apart from stopping what is running: the log
                reports the board's work rather than commanding it. */}
            {runs.length === 0 ? (
              <p className="mt-2 font-mono text-[0.62rem] text-ink-soft leading-snug">
                No runs yet — this card starts working when it reaches In Progress with an
                assignee.
              </p>
            ) : (
              <ul className="mt-2 flex flex-col">
                {runs.map((run) => (
                  <RunRow
                    key={run.attempt}
                    run={run}
                    busy={saving}
                    onCancel={() => {
                      void stopRun();
                    }}
                  />
                ))}
              </ul>
            )}
          </section>

          {/* Shown even at zero: "this card has cost nothing yet" is an
              answer, and a block that appears only once there is a bill
              cannot be looked at to ask the question. */}
          <section className={railBox}>
            <h2 className={railLabel}>Tokens</h2>
            <div className="mt-1.5">
              <Row label="input / output">
                <span className="tabular-nums">
                  {formatTokens(runLog?.total_input_tokens ?? 0)} /{' '}
                  {formatTokens(runLog?.total_output_tokens ?? 0)}
                </span>
              </Row>
              <Row label="cost (all runs)">
                <span className="tabular-nums">{formatUsd(runLog?.total_cost_micros ?? 0)}</span>
              </Row>
            </div>
          </section>
        </aside>
      </div>
    </div>
  );
}
