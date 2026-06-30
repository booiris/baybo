import { useCallback, useEffect, useRef, useState } from 'react';
import { RiFocus3Line, RiCloseLine } from 'react-icons/ri';
import { useAdminClient } from '../../api/auth';
import type { components } from '../../api/schema';
import { fmtTokens, statusLabel, statusPillClass } from '../goalFormat';

type GoalItem = components['schemas']['GoalItem'];

// Goals move slowly — a token tick lands once per continuation turn (seconds to
// minutes). Poll an *active* goal at a modest cadence; back off hard when there's
// nothing live to watch (no goal, or a stopped one) so an idle chat isn't
// hammering the endpoint.
const POLL_ACTIVE_MS = 8000;
const POLL_IDLE_MS = 20000;

/**
 * Goal strip under the chat header; renders nothing when no goal is set. The
 * only control is a close (✕) on a *completed* goal, which clears it (the goal
 * is done, so this removes the row and the banner with it). Pause / resume are
 * issued as `/goal …` slash commands from the composer.
 */
export function GoalBanner({
  sessionId,
  refreshSignal,
}: {
  sessionId: string | undefined;
  // Incremented by ChatPage when a `/goal …` send goes out (see its
  // `goalRefresh`); each bump triggers a prompt retry-burst refetch.
  refreshSignal: number;
}) {
  const client = useAdminClient();
  const [goal, setGoal] = useState<GoalItem | null>(null);
  const [tick, setTick] = useState(0);
  const lastSignalRef = useRef(refreshSignal);

  const fetchGoal = useCallback(
    async (canceled: () => boolean) => {
      if (!sessionId) {
        setGoal(null);
        return;
      }
      try {
        const { data, response } = await client.GET('/v1/chat/sessions/{session_id}/goal', {
          params: { path: { session_id: sessionId } },
        });
        if (canceled() || !response.ok) return;
        setGoal(data?.goal ?? null);
      } catch {
        // Transient fetch failure: keep the last known goal rather than flicker.
      }
    },
    [client, sessionId],
  );

  useEffect(() => {
    let canceled = false;
    void fetchGoal(() => canceled);
    return () => {
      canceled = true;
    };
  }, [fetchGoal, tick]);

  useEffect(() => {
    const interval = goal?.status === 'active' ? POLL_ACTIVE_MS : POLL_IDLE_MS;
    const handle = window.setInterval(() => setTick((t) => t + 1), interval);
    return () => window.clearInterval(handle);
  }, [goal?.status]);

  // A `/goal …` send just went out: the goal row is created only after the
  // message round-trips, so refetch a few times to absorb that latency instead
  // of waiting for the idle poll. The ref-compare fires the burst only on a real
  // signal increment — not on mount or a `fetchGoal` identity change (session
  // switch), which the deps would otherwise re-trigger.
  useEffect(() => {
    if (refreshSignal === lastSignalRef.current) return;
    lastSignalRef.current = refreshSignal;
    let canceled = false;
    const timers = [250, 800, 2000].map((delay) =>
      window.setTimeout(() => void fetchGoal(() => canceled), delay),
    );
    return () => {
      canceled = true;
      timers.forEach((t) => window.clearTimeout(t));
    };
  }, [refreshSignal, fetchGoal]);

  if (!goal) return null;

  const isComplete = goal.status === 'complete';

  const handleClear = async () => {
    if (!sessionId) return;
    setGoal(null); // optimistic — the banner closes immediately
    try {
      await client.DELETE('/v1/chat/sessions/{session_id}/goal', {
        params: { path: { session_id: sessionId } },
      });
    } catch {
      void fetchGoal(() => false); // restore on failure
    }
  };

  const pill = statusPillClass(goal.status);
  const iconBtn =
    'flex h-7 w-7 items-center justify-center rounded-brutal border-2 border-black transition-[transform] duration-100 hover:bg-brand active:translate-x-[1px] active:translate-y-[1px] cursor-pointer';

  return (
    <div className="px-4 py-2 border-b-2 border-black bg-canvas flex items-center gap-3 text-ink">
      <RiFocus3Line className="text-lg shrink-0 text-ink-soft" />
      <span
        className={`shrink-0 rounded-brutal border-2 border-black px-2 py-0.5 text-[0.65rem] font-bold uppercase tracking-wider ${pill}`}
      >
        {statusLabel(goal.status)}
      </span>
      <span className="flex-1 min-w-0 truncate text-sm font-medium" title={goal.objective}>
        {goal.objective}
      </span>
      <span className="shrink-0 font-mono text-[0.7rem] text-ink-soft">{fmtTokens(goal)} tokens</span>
      {isComplete ? (
        <button
          type="button"
          title="Clear the completed goal"
          className={`${iconBtn} shrink-0`}
          onClick={() => void handleClear()}
        >
          <RiCloseLine className="text-base" />
        </button>
      ) : null}
    </div>
  );
}
