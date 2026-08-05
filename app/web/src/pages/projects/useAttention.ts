import { useEffect, useState } from 'react';

import { useAdminClient } from '../../api/auth';
import type { paths } from '../../api/schema';

/**
 * Derived from the path rather than from `components`: the DTO is only ever
 * returned inside a list, so utoipa inlines it and it is not a named
 * schema. Naming it in the API purely to give the client a shorter import
 * would be distorting the surface for the client's convenience.
 */
export type ProjectAttention =
  paths['/v1/projects/attention']['get']['responses'][200]['content']['application/json']['items'][number];

/**
 * How often the rail re-asks what is waiting.
 *
 * A poll rather than a subscription: the rail is mounted on every route,
 * including ones with no board socket, and the answer changes on a human
 * timescale — an approval prompt self-denies after five minutes, a held run
 * waits for a budget edit. A minute is well inside both.
 */
const POLL_MS = 60_000;

/**
 * The boards with something stuck on the operator.
 *
 * Every count is derived from durable state plus the live approval queue,
 * so there is nothing to mark read: each entry disappears when the operator
 * does the thing only they can do — answer the prompt, raise the budget,
 * retry or cancel the failed card.
 */
export function useAttention(): ProjectAttention[] {
  const client = useAdminClient();
  const [boards, setBoards] = useState<ProjectAttention[]>([]);

  useEffect(() => {
    let canceled = false;
    async function poll() {
      try {
        const { data, error, response } = await client.GET('/v1/projects/attention');
        if (canceled) return;
        // Silent on failure, and it keeps the last answer rather than
        // clearing: a badge that blinks off on one dropped request is a
        // badge nobody trusts. A dead token is the auth layer's problem,
        // not the rail's.
        if (error !== undefined || !response.ok) return;
        setBoards(data.items);
      } catch {
        // Same reasoning: a network blip must not clear the badge.
      }
    }
    void poll();
    const timer = setInterval(() => {
      void poll();
    }, POLL_MS);
    return () => {
      canceled = true;
      clearInterval(timer);
    };
  }, [client]);

  return boards;
}

/**
 * What the rail's Projects entry shows: the number of **boards** with
 * something waiting, not the number of things.
 *
 * A total across boards cannot be discharged by clicking the rail — the
 * entry opens exactly one board (there is no project list page), so you
 * would deal with two of seven and watch the badge sit at five with no
 * explanation. A board count decomposes into the affordance that already
 * exists for reaching the others: the switcher dropdown.
 */
export function boardsNeedingAttention(boards: ProjectAttention[]): number {
  return boards.length;
}

/** One board's own counts, for the switcher row. */
export function attentionFor(
  boards: ProjectAttention[],
  projectId: string,
): ProjectAttention | null {
  return boards.find((board) => board.project_id === projectId) ?? null;
}

/** A sentence a person can act on, for the badge's tooltip. */
export function attentionSummary(boards: ProjectAttention[]): string {
  if (boards.length === 0) return 'Nothing is waiting on you';
  const parts: string[] = [];
  const sum = (pick: (b: ProjectAttention) => number) =>
    boards.reduce((total, board) => total + pick(board), 0);
  const approvals = sum((b) => b.approvals);
  const held = sum((b) => b.held);
  const failed = sum((b) => b.failed);
  const unread = sum((b) => b.unread);
  if (approvals > 0) parts.push(`${approvals} waiting on approval`);
  if (held > 0) parts.push(`${held} held on budget`);
  if (failed > 0) parts.push(`${failed} failed`);
  if (unread > 0) parts.push(`${unread} new since you looked`);
  const where = boards.length === 1 ? boards[0].name : `${boards.length} boards`;
  return `${where}: ${parts.join(', ')}`;
}
