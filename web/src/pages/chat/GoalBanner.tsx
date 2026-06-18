import { useCallback, useEffect, useState } from 'react';
import {
  RiFocus3Line,
  RiPauseCircleLine,
  RiPlayCircleLine,
  RiCloseCircleLine,
} from 'react-icons/ri';
import { useAdminClient } from '../../api/auth';
import type { components } from '../../api/schema';

type GoalItem = components['schemas']['GoalItem'];

const POLL_MS = 4000;

const STATUS_STYLE: Record<string, string> = {
  active: 'bg-brand text-ink',
  complete: 'bg-ok text-white',
  blocked: 'bg-err text-white',
  paused: 'bg-selected text-ink',
  budget_limited: 'bg-warn text-white',
  spend_capped: 'bg-warn text-white',
};

function tokensLabel(goal: GoalItem): string {
  const used = goal.tokens_used.toLocaleString();
  if (goal.token_budget == null) return `${used} tokens`;
  return `${used} / ${goal.token_budget.toLocaleString()} tokens`;
}

/**
 * Compact strip under the chat header showing the session's autonomous goal:
 * objective, status pill, token usage, and pause/resume/clear controls. Renders
 * nothing when no goal is set. Controls are issued as `/goal …` slash commands
 * through `onCommand` (the normal send path) so resume correctly re-arms the
 * continuation loop, not just the durable status.
 */
export function GoalBanner({
  sessionId,
  onCommand,
}: {
  sessionId: string | undefined;
  onCommand: (command: string) => void;
}) {
  const client = useAdminClient();
  const [goal, setGoal] = useState<GoalItem | null>(null);
  const [tick, setTick] = useState(0);

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
    const handle = window.setInterval(() => setTick((t) => t + 1), POLL_MS);
    return () => window.clearInterval(handle);
  }, []);

  if (!goal) return null;

  const isActive = goal.status === 'active';
  const isComplete = goal.status === 'complete';
  const pill = STATUS_STYLE[goal.status] ?? 'bg-white text-ink';
  const iconBtn =
    'flex h-7 w-7 items-center justify-center rounded-brutal border-2 border-black transition-[transform] duration-100 hover:bg-brand active:translate-x-[1px] active:translate-y-[1px] cursor-pointer';

  return (
    <div className="px-4 py-2 border-b-2 border-black bg-canvas flex items-center gap-3 text-ink">
      <RiFocus3Line className="text-lg shrink-0 text-ink-soft" />
      <span
        className={`shrink-0 rounded-brutal border-2 border-black px-2 py-0.5 text-[0.65rem] font-bold uppercase tracking-wider ${pill}`}
      >
        {goal.status.replace('_', ' ')}
      </span>
      <span className="flex-1 min-w-0 truncate text-sm font-medium" title={goal.objective}>
        {goal.objective}
      </span>
      <span className="shrink-0 font-mono text-[0.7rem] text-ink-soft">{tokensLabel(goal)}</span>
      <div className="shrink-0 flex items-center gap-1.5">
        {isActive ? (
          <button
            type="button"
            title="Pause the goal loop"
            className={iconBtn}
            onClick={() => onCommand('/goal pause')}
          >
            <RiPauseCircleLine className="text-base" />
          </button>
        ) : !isComplete ? (
          <button
            type="button"
            title="Resume the goal"
            className={iconBtn}
            onClick={() => onCommand('/goal resume')}
          >
            <RiPlayCircleLine className="text-base" />
          </button>
        ) : null}
        <button
          type="button"
          title="Clear (delete) the goal"
          className={`${iconBtn} text-err hover:bg-err hover:text-white`}
          onClick={() => onCommand('/goal clear')}
        >
          <RiCloseCircleLine className="text-base" />
        </button>
      </div>
    </div>
  );
}
