import { useCallback, useEffect, useRef, useState } from 'react';
import { Link, useParams } from 'react-router-dom';
import { RiArrowLeftLine, RiLoader4Line } from 'react-icons/ri';

import { useAdminClient, useAuth } from '../../api/auth';
import { Button } from '../../components/Button';
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
  retryRun,
} from './api';
import {
  COLUMNS,
  COLUMN_LABEL,
  HELD_RUN_NOTE,
  PRIORITIES,
  assignableAgents,
  retryRejection,
  runDuration,
  unsettledRun,
  type Agent,
  type Issue,
  type IssueRun,
  type IssuePriority,
  type IssueStatus,
} from './boardModel';
import { SubIssues } from './SubIssues';
import { Timeline } from './Timeline';
import type { IssueEvent } from './timelineModel';
import { useBoardStream } from './useBoardStream';

const PRIORITY_LABEL: Record<IssuePriority, string> = {
  urgent: 'Urgent',
  high: 'High',
  medium: 'Medium',
  low: 'Low',
  none: 'No priority',
};

const railLabel = 'font-mono text-[0.6rem] font-bold uppercase tracking-wider text-ink-soft';

const RUN_TONE: Record<IssueRun['status'], string> = {
  held: 'border-warn/50 bg-warn/12 text-warn',
  queued: 'border-black/35 bg-canvas text-ink-soft',
  running: 'border-black bg-brand/40 text-ink',
  done: 'border-ok/50 bg-ok/15 text-ok',
  failed: 'border-err/45 bg-err/12 text-err',
  cancelled: 'border-black/35 bg-canvas text-ink-soft',
};

const RUN_TRIGGER_LABEL: Record<IssueRun['trigger'], string> = {
  started: 'started',
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
  const live = run.status === 'queued' || run.status === 'running';
  return (
    <li className="flex items-center gap-2 py-1.5 border-b border-black/15 last:border-0 font-mono text-[0.62rem]">
      <span className="font-bold">#{run.attempt}</span>
      <span className="text-ink-soft">{RUN_TRIGGER_LABEL[run.trigger]}</span>
      <span className={`rounded-full border px-2 font-bold ${RUN_TONE[run.status]}`}>
        {run.status}
      </span>
      {duration != null ? <span className="text-ink-soft">{duration}</span> : null}
      <span className="ml-auto flex items-center gap-2">
        {live ? (
          <button
            type="button"
            className="text-err underline cursor-pointer disabled:opacity-50"
            disabled={busy}
            onClick={onCancel}
          >
            stop
          </button>
        ) : null}
        {run.session_id != null ? (
          <a className="text-info underline" href={`#/traces/${encodeURIComponent(run.session_id)}`}>
            transcript
          </a>
        ) : null}
      </span>
    </li>
  );
}

const railSelect =
  'w-full mt-1 px-2 py-1 bg-surface border-2 border-black rounded-md font-mono text-[0.72rem] outline-none cursor-pointer';

export function IssueDetailPage() {
  const { pid, num } = useParams<{ pid: string; num: string }>();
  const projectId = pid ?? '';
  const number = Number(num ?? '');
  const client = useAdminClient();
  const { logout } = useAuth();

  const [issue, setIssue] = useState<Issue | null>(null);
  const [board, setBoard] = useState<Issue[]>([]);
  const [agents, setAgents] = useState<Agent[]>([]);
  const [runs, setRuns] = useState<IssueRun[]>([]);
  const [events, setEvents] = useState<IssueEvent[]>([]);
  const [refreshKey, setRefreshKey] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [title, setTitle] = useState('');
  const [description, setDescription] = useState('');
  const [saving, setSaving] = useState(false);
  const serverProse = useRef({ title: '', description: '' });

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
      if (runsOutcome.kind === 'ok') setRuns(runsOutcome.value);
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
      // Live refreshes must not overwrite an edit in progress.
      setTitle((current) => (current === serverProse.current.title ? outcome.value.title : current));
      setDescription((current) =>
        current === serverProse.current.description ? outcome.value.description : current,
      );
      serverProse.current = {
        title: outcome.value.title,
        description: outcome.value.description,
      };
      setError(null);
      setLoading(false);
    }
    void load();
    return () => {
      canceled = true;
    };
  }, [client, logout, number, projectId, refreshKey]);

  useBoardStream(
    projectId,
    number,
    useCallback(() => {
      setRefreshKey((key) => key + 1);
    }, []),
  );

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

  const runAgain = useCallback(async () => {
    setSaving(true);
    const outcome = await retryRun(client, projectId, number);
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
  const inFlight = live !== null && live.status !== 'held';
  const retryRefusal = retryRejection(issue);
  const heldNote = live?.status === 'held' && retryRefusal === null ? HELD_RUN_NOTE : null;
  const dirty = title !== issue.title || description !== issue.description;
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
      </header>

      <div className="flex-1 min-h-0 flex overflow-hidden bg-surface">
        <main className="flex-1 min-w-0 overflow-y-auto p-5">
          {error != null ? (
            <div className="mb-4 bg-white border-2 border-err text-err rounded-md px-3 py-2 font-mono text-[0.78rem] break-words">
              {error}
            </div>
          ) : null}

          <input
            className={`w-full bg-transparent font-mono text-[1.15rem] font-bold outline-none ${
              cancelled ? 'line-through text-ink-soft' : ''
            }`}
            value={title}
            onChange={(event) => {
              setTitle(event.target.value);
            }}
          />
          <textarea
            className="mt-3 w-full min-h-[160px] bg-canvas border border-dashed border-black/30 rounded-md px-3 py-2 font-sans text-[0.85rem] outline-none resize-y"
            placeholder="What, why, and what done looks like…"
            value={description}
            onChange={(event) => {
              setDescription(event.target.value);
            }}
          />
          <div className="mt-3 flex items-center gap-2">
            <Button
              className="!px-4 !py-1.5 !text-[0.75rem]"
              variant="primary"
              disabled={!dirty || saving}
              onClick={() => {
                void apply({ title, description });
              }}
            >
              {saving ? 'Saving…' : 'Save'}
            </Button>
            {dirty ? (
              <button
                type="button"
                className="font-mono text-[0.7rem] text-ink-soft underline cursor-pointer"
                onClick={() => {
                  setTitle(issue.title);
                  setDescription(issue.description);
                }}
              >
                Discard
              </button>
            ) : null}
          </div>

          <SubIssues
            projectId={projectId}
            children={children}
            team={agents}
            disabled={saving}
            onStatus={(childNumber, status) => {
              void moveChild(childNumber, status);
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

        <aside className="w-[300px] shrink-0 border-l-2 border-black bg-canvas p-4 flex flex-col gap-4 overflow-y-auto">
          <section className="border-2 border-black rounded-md bg-surface p-3">
            <h2 className={railLabel}>Properties</h2>
            <label className="block mt-2">
              <span className={railLabel}>Status</span>
              <select
                className={railSelect}
                value={issue.status}
                disabled={saving}
                onChange={(event) => {
                  void relocate(event.target.value as IssueStatus);
                }}
              >
                {COLUMNS.map((status) => (
                  <option key={status} value={status}>
                    {COLUMN_LABEL[status]}
                  </option>
                ))}
              </select>
            </label>
            <label className="block mt-2">
              <span className={railLabel}>Assignee</span>
              <select
                className={railSelect}
                value={issue.assignee ?? ''}
                disabled={saving}
                onChange={(event) => {
                  const picked = event.target.value;
                  void apply({ assignee: picked.length > 0 ? picked : null });
                }}
              >
                <option value="">Unassigned</option>
                {agents.map((agent) => (
                  <option key={agent.id} value={agent.id}>
                    @{agent.handle} — {agent.name}
                  </option>
                ))}
              </select>
            </label>
            <label className="block mt-2">
              <span className={railLabel}>Priority</span>
              <select
                className={railSelect}
                value={issue.priority}
                onChange={(event) => {
                  void apply({ priority: event.target.value as IssuePriority });
                }}
              >
                {PRIORITIES.map((priority) => (
                  <option key={priority} value={priority}>
                    {PRIORITY_LABEL[priority]}
                  </option>
                ))}
              </select>
            </label>
          </section>

          {issue.branch != null ? (
            <section className="border-2 border-black rounded-md bg-surface p-3">
              <h2 className={railLabel}>Branch</h2>
              <div className="mt-1 flex items-center gap-2">
                <code className="min-w-0 flex-1 truncate font-mono text-[0.7rem]" title={issue.branch}>
                  {issue.branch}
                </code>
                <button
                  type="button"
                  className="shrink-0 font-mono text-[0.66rem] underline cursor-pointer"
                  onClick={() => {
                    if (issue.branch != null) void navigator.clipboard.writeText(issue.branch);
                  }}
                >
                  copy
                </button>
              </div>
            </section>
          ) : null}

          <section className="border-2 border-black rounded-md bg-surface p-3">
            <h2 className={railLabel}>Execution log</h2>
            {inFlight ? null : (
              <>
                <button
                  type="button"
                  className="mt-1 font-mono text-[0.66rem] font-bold underline cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                  disabled={saving || retryRefusal !== null}
                  onClick={() => {
                    void runAgain();
                  }}
                >
                  {runs.length === 0 ? 'Run it now' : 'Run it again'}
                </button>
                {(retryRefusal ?? heldNote) !== null ? (
                  <p className="mt-1 font-mono text-[0.62rem] text-ink-soft leading-snug break-words">
                    {retryRefusal ?? heldNote}
                  </p>
                ) : null}
              </>
            )}
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

          <section className="border-2 border-black rounded-md bg-surface p-3">
            <h2 className={railLabel}>Actions</h2>
            <div className="mt-2 flex flex-col gap-2">
              {blocked ? (
                <>
                  <p className="border border-warn/50 bg-warn/10 text-warn rounded px-2 py-1 font-mono text-[0.66rem] break-words">
                    ⚑ {issue.blocked_reason}
                  </p>
                  <button
                    type="button"
                    className="text-left font-mono text-[0.7rem] underline cursor-pointer"
                    onClick={() => {
                      void apply({ blocked_reason: null });
                    }}
                  >
                    Unblock
                  </button>
                </>
              ) : (
                <button
                  type="button"
                  className="text-left font-mono text-[0.7rem] underline cursor-pointer"
                  onClick={() => {
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
                className={`text-left font-mono text-[0.7rem] underline cursor-pointer ${
                  cancelled ? '' : 'text-err'
                }`}
                onClick={() => {
                  void apply({ cancelled: !cancelled });
                }}
              >
                {cancelled ? 'Reopen issue' : 'Cancel issue'}
              </button>
            </div>
            <p className="mt-2 font-mono text-[0.58rem] text-ink-soft leading-snug">
              Cancelling keeps the issue, its number and its history — it just stops counting as
              live work.
            </p>
          </section>
        </aside>
      </div>
    </div>
  );
}
