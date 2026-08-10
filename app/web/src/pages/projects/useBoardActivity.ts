import { useCallback, useEffect, useState } from 'react';

import { useAdminClient } from '../../api/auth';
import type { paths } from '../../api/schema';

export type BoardActivity =
  paths['/v1/projects/activity']['get']['responses'][200]['content']['application/json']['items'][number];

/// Midnight this morning, on the operator's clock. Sent to the server
/// rather than left to it, because "today's burn" is a question about the
/// reader's day and a gateway in another timezone would answer a different
/// one.
function startOfToday(now: number): number {
  const midnight = new Date(now);
  midnight.setHours(0, 0, 0, 0);
  return midnight.getTime();
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
        params: { query: { since_ms: startOfToday(Date.now()) } },
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

export function burnIsNearLimit(
  burnMicros: number,
  budgetMicros: number | null | undefined,
): boolean {
  if (budgetMicros == null || budgetMicros <= 0) return false;
  return burnMicros / budgetMicros >= WARN_AT;
}
