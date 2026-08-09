import { useEffect, useState } from 'react';

import { useAdminClient } from '../../api/auth';
import type { paths } from '../../api/schema';

export type ProjectAttention =
  paths['/v1/projects/attention']['get']['responses'][200]['content']['application/json']['items'][number];

const POLL_MS = 60_000;

export function useAttention(): ProjectAttention[] {
  const client = useAdminClient();
  const [boards, setBoards] = useState<ProjectAttention[]>([]);

  useEffect(() => {
    let canceled = false;
    async function poll() {
      try {
        const { data, error, response } = await client.GET('/v1/projects/attention');
        if (canceled) return;
        if (error !== undefined || !response.ok) return;
        setBoards(data.items);
      } catch {
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

export function boardsNeedingAttention(boards: ProjectAttention[]): number {
  return boards.length;
}

export function attentionFor(
  boards: ProjectAttention[],
  projectId: string,
): ProjectAttention | null {
  return boards.find((board) => board.project_id === projectId) ?? null;
}

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
