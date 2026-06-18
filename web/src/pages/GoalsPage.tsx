import { useCallback, useEffect, useState } from 'react';
import {
  RiRefreshLine,
  RiLoader4Line,
  RiPauseCircleLine,
  RiDeleteBin6Line,
  RiFocus3Line,
} from 'react-icons/ri';
import { Button } from '../components/Button';
import { useAdminClient, useAuth } from '../api/auth';
import type { components } from '../api/schema';
import { useMockMode, MOCK_GOALS } from '../api/mock';

type GoalsResponse = components['schemas']['GoalsResponse'];
type GoalItem = components['schemas']['GoalItem'];

// Goals are long-lived but their token usage ticks every continuation turn,
// so poll while the tab is open.
const POLL_MS = 4000;

const thCell =
  'px-6 py-4 text-left font-bold text-[0.85rem] uppercase tracking-wider border-b-2 border-black sticky top-0 z-10 bg-white';

const EMPTY: GoalsResponse = { goals: [] };

// Status pill colours, dark-on-warm to match the design system.
const STATUS_STYLE: Record<string, string> = {
  active: 'bg-brand text-ink',
  complete: 'bg-ok text-white',
  blocked: 'bg-err text-white',
  paused: 'bg-selected text-ink',
  budget_limited: 'bg-warn text-white',
  spend_capped: 'bg-warn text-white',
};

function statusPill(status: string) {
  const style = STATUS_STYLE[status] ?? 'bg-white text-ink';
  return (
    <span
      className={`inline-block rounded-brutal border-2 border-black px-2 py-0.5 text-[0.7rem] font-bold uppercase tracking-wider ${style}`}
    >
      {status.replace('_', ' ')}
    </span>
  );
}

function fmtDuration(seconds: number): string {
  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = seconds % 60;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}

function fmtTokens(goal: GoalItem): string {
  const used = goal.tokens_used.toLocaleString();
  if (goal.token_budget == null) return used;
  return `${used} / ${goal.token_budget.toLocaleString()}`;
}

export function GoalsPage() {
  const isMock = useMockMode();
  const client = useAdminClient();
  const { logout } = useAuth();

  const [data, setData] = useState<GoalsResponse>(EMPTY);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);
  const [busy, setBusy] = useState<string | null>(null);

  const fetchData = useCallback(
    async (canceled: () => boolean) => {
      if (isMock) {
        setData(MOCK_GOALS);
        setLoading(false);
        setError(null);
        return;
      }
      setLoading(true);
      try {
        const { data: body, error: apiError, response } = await client.GET('/v1/goals');
        if (canceled()) return;
        if (response.status === 401) {
          logout();
          return;
        }
        if (apiError || !response.ok) {
          setError(apiError?.error || `HTTP Error ${response.status}`);
          return;
        }
        setData(body ?? EMPTY);
        setError(null);
      } catch (e) {
        if (canceled()) return;
        setError(
          e instanceof Error ? `Network error: ${e.message}` : 'Network error contacting gateway',
        );
      } finally {
        if (!canceled()) setLoading(false);
      }
    },
    [client, logout, isMock],
  );

  useEffect(() => {
    let canceled = false;
    void fetchData(() => canceled);
    return () => {
      canceled = true;
    };
  }, [fetchData, refreshKey]);

  useEffect(() => {
    if (isMock) return;
    const handle = window.setInterval(() => setRefreshKey((k) => k + 1), POLL_MS);
    return () => window.clearInterval(handle);
  }, [isMock]);

  const pauseGoal = useCallback(
    async (sessionId: string) => {
      if (isMock) return;
      setBusy(`pause:${sessionId}`);
      try {
        await client.POST('/v1/chat/sessions/{session_id}/goal/pause', {
          params: { path: { session_id: sessionId } },
        });
        setRefreshKey((k) => k + 1);
      } catch (e) {
        setError(e instanceof Error ? `Network error: ${e.message}` : 'Failed to pause goal');
      } finally {
        setBusy(null);
      }
    },
    [client, isMock],
  );

  const clearGoal = useCallback(
    async (sessionId: string) => {
      if (isMock) return;
      setBusy(`clear:${sessionId}`);
      try {
        await client.DELETE('/v1/chat/sessions/{session_id}/goal', {
          params: { path: { session_id: sessionId } },
        });
        setRefreshKey((k) => k + 1);
      } catch (e) {
        setError(e instanceof Error ? `Network error: ${e.message}` : 'Failed to clear goal');
      } finally {
        setBusy(null);
      }
    },
    [client, isMock],
  );

  const goals: GoalItem[] = data.goals;

  return (
    <div className="p-5 h-full flex flex-col overflow-hidden">
      <div className="flex justify-between items-start mb-3">
        <h2 className="text-[1.7rem] font-bold uppercase -tracking-[0.05em] mb-1 flex items-center gap-2">
          <RiFocus3Line /> GOALS
        </h2>
        <Button
          onClick={() => setRefreshKey((k) => k + 1)}
          disabled={loading || isMock}
          className="!py-2 !px-4 !text-[0.9rem] h-10 w-[120px] justify-center gap-1.5"
        >
          <RiRefreshLine className="text-lg shrink-0" /> Refresh
        </Button>
      </div>

      {error && (
        <div className="mb-4 bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm">
          {error}
        </div>
      )}

      <div className="flex-1 flex flex-col min-h-0 bg-white border-[3px] border-black rounded-md shadow-brutal">
        <div className="flex-1 overflow-auto overscroll-none">
          <table className="w-full border-separate border-spacing-0">
            <thead>
              <tr>
                <th className={thCell}>Objective</th>
                <th className={`${thCell} w-[140px]`}>Status</th>
                <th className={`${thCell} w-[200px]`}>Tokens</th>
                <th className={`${thCell} w-[110px]`}>Time</th>
                <th className={`${thCell} w-[200px]`}>Session</th>
                <th className={`${thCell} w-[150px]`}>Actions</th>
              </tr>
            </thead>
            <tbody>
              {goals.length === 0 && !loading && (
                <tr>
                  <td colSpan={6} className="px-6 py-10 text-center text-ink-soft text-[0.9rem]">
                    No goals set. Start one in a chat with <code>/goal &lt;objective&gt;</code>.
                  </td>
                </tr>
              )}
              {goals.map((goal) => {
                const cell = 'px-6 py-4 align-middle border-b border-black';
                const isActive = goal.status === 'active';
                return (
                  <tr key={goal.session_id} className="hover:bg-gray-50">
                    <td className={cell}>
                      <span className="text-[0.9rem] line-clamp-2">{goal.objective}</span>
                    </td>
                    <td className={cell}>{statusPill(goal.status)}</td>
                    <td className={`${cell} font-mono text-[0.8rem]`}>{fmtTokens(goal)}</td>
                    <td className={`${cell} font-mono text-[0.8rem]`}>
                      {fmtDuration(goal.time_used_seconds)}
                    </td>
                    <td className={cell}>
                      <code className="font-mono text-[0.8rem] text-ink-soft break-all">
                        {goal.session_id}
                      </code>
                    </td>
                    <td className={cell}>
                      <div className="flex items-center gap-2">
                        <button
                          type="button"
                          title="Pause the goal loop"
                          disabled={!isActive || busy != null || isMock}
                          onClick={() => void pauseGoal(goal.session_id)}
                          className="flex h-8 w-8 items-center justify-center rounded-brutal border-2 border-black text-ink transition-[transform,box-shadow] duration-100 hover:bg-brand disabled:opacity-30 disabled:cursor-not-allowed active:translate-x-[1px] active:translate-y-[1px]"
                        >
                          <RiPauseCircleLine className="text-lg" />
                        </button>
                        <button
                          type="button"
                          title="Clear (delete) the goal"
                          disabled={busy != null || isMock}
                          onClick={() => void clearGoal(goal.session_id)}
                          className="flex h-8 w-8 items-center justify-center rounded-brutal border-2 border-black text-err transition-[transform,box-shadow] duration-100 hover:bg-err hover:text-white disabled:opacity-30 disabled:cursor-not-allowed active:translate-x-[1px] active:translate-y-[1px]"
                        >
                          <RiDeleteBin6Line className="text-lg" />
                        </button>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>

        <div className="flex justify-between items-center px-4 py-3 border-t-2 border-black bg-white">
          <span className="text-[0.85rem] text-ink-soft">
            {loading && goals.length === 0 ? (
              <span className="flex items-center gap-2">
                <RiLoader4Line className="animate-spin" /> Loading…
              </span>
            ) : (
              `${goals.length} goal${goals.length === 1 ? '' : 's'}`
            )}
          </span>
        </div>
      </div>
    </div>
  );
}
