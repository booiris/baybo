import { useCallback, useEffect, useState } from 'react';

import { useAdminClient } from '../../api/auth';
import type { paths } from '../../api/schema';

export type BoardActivity =
  paths['/v1/projects/activity']['get']['responses'][200]['content']['application/json']['items'][number];

/// The start of the **budget's** day, which is UTC midnight — not the
/// reader's.
///
/// This number's whole job is to predict the gate: it is shown against the
/// ceiling that stops the board, and `budget::day_start` measures the UTC
/// calendar day. Asked for the reader's midnight instead it answered a
/// different question in the same units, so at UTC+8 the dropdown spent
/// eight hours a day reporting spend the gate was not counting — a board
/// reading well over its ceiling while it happily started work, and the
/// reverse the rest of the time.
export function startOfBudgetDay(now: number): number {
  const at = new Date(now);
  return Date.UTC(at.getUTCFullYear(), at.getUTCMonth(), at.getUTCDate());
}

/// Live working counts and today's burn, per board.
///
/// Fetched when the switcher opens and refreshed on the board's own change
/// signal — never polled. A dropdown that is shut costs nothing, and a
/// number that only moves when something happened is the cheap half of
/// decision ⑦; the expensive half would be a timer nobody is watching.
export function useBoardActivity(enabled: boolean, refreshKey: number): BoardActivity[] {
  const client = useAdminClient();
  const [boards, setBoards] = useState<BoardActivity[]>([]);

  const load = useCallback(async () => {
    try {
      const { data, error, response } = await client.GET('/v1/projects/activity', {
        params: { query: { since_ms: startOfBudgetDay(Date.now()) } },
      });
      if (error !== undefined || !response.ok) return null;
      return data.items;
    } catch {
      // A dropdown that cannot reach the gateway shows the boards without
      // their numbers, which is strictly better than showing neither.
      return null;
    }
  }, [client]);

  useEffect(() => {
    if (!enabled) return;
    let canceled = false;
    void load().then((items) => {
      if (!canceled && items !== null) setBoards(items);
    });
    return () => {
      canceled = true;
    };
  }, [enabled, load, refreshKey]);

  return boards;
}

export function activityFor(boards: BoardActivity[], projectId: string): BoardActivity | null {
  return boards.find((board) => board.project_id === projectId) ?? null;
}

/// Whether the day's spend is close enough to the ceiling to say so. The
/// threshold lives here rather than in the markup because the dropdown and
/// any later banner have to agree about when a board is nearly out.
const WARN_AT = 0.95;

/// Where the day's spend sits against the ceiling, in either unit.
///
/// `over` is its own answer and not the far end of `near`. With only a
/// boolean, a board six times past its ceiling — every run on it held, and
/// no press on the board able to start one — still read "close to the daily
/// ceiling", which is the one board where that sentence is wrong in the
/// direction that matters.
///
/// A zero ceiling is `over` from the first second of the day: the gate is
/// `spent >= limit`, so a paused board is exhausted rather than untouched.
export type BurnState = 'ok' | 'near' | 'over';

export function burnState(used: number, ceiling: number | null | undefined): BurnState {
  if (ceiling == null) return 'ok';
  if (used >= ceiling) return 'over';
  return used / ceiling >= WARN_AT ? 'near' : 'ok';
}
