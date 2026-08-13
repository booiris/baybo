import { useEffect, useState } from 'react';

import { useAdminClient } from '../../api/auth';
import type { AdminClient } from '../../api/client';
import type { paths } from '../../api/schema';

export type ProjectAttention =
  paths['/v1/projects/attention']['get']['responses'][200]['content']['application/json']['items'][number];

const POLL_MS = 60_000;

/// One cache, one timer, for every component that shows the badge.
///
/// The rail and the switcher used to hold a `useState` and a `setInterval`
/// each, which made them two answers to one question refreshed on different
/// phases of the same minute. Worse, the minute was the *only* refresh:
/// nothing re-read the count after the operator had just discharged it, so
/// a dot survived everything they did for up to a minute and read as one
/// that could not be cleared at all. [`invalidateAttention`] is the missing
/// half — the act that clears a signal also asks the server again.
const store = {
  boards: [] as ProjectAttention[],
  listeners: new Set<() => void>(),
  timer: null as ReturnType<typeof setInterval> | null,
  /// Whose counts these are. Module state outlives a logout, and the rail
  /// seeds its first render from the cache — without this, logging into a
  /// second gateway paints the previous one's boards until the first
  /// response lands.
  client: null as AdminClient | null,
  /// The poll in flight, so a burst of invalidations — a board change and
  /// a card being read in the same tick — costs one request rather than one
  /// each.
  inFlight: null as Promise<void> | null,
  /// An invalidation raised while a poll was already open. The answer on
  /// the wire was computed *before* the act that raised it, so joining that
  /// request would discharge the invalidation with a stale snapshot and
  /// nothing would ask again until the timer. One more has to follow.
  dirty: false,
};

function publish(boards: ProjectAttention[]) {
  store.boards = boards;
  for (const listener of store.listeners) listener();
}

async function poll(client: AdminClient): Promise<void> {
  try {
    const { data, error, response } = await client.GET('/v1/projects/attention');
    if (error !== undefined || !response.ok) return;
    publish(data.items);
  } catch {
    // A dashboard that cannot reach the gateway keeps the counts it has:
    // the next poll replaces them wholesale, and an empty list is how the
    // server says "nothing waiting", so a dropped request must not be
    // allowed to say it on the server's behalf.
  }
}

function refresh(client: AdminClient): Promise<void> {
  if (store.client !== client) {
    // A different gateway or a different token: the cache describes boards
    // this client may not even have. Dropping `inFlight` with it matters as
    // much as dropping the counts — joining the old client's request would
    // publish the old host's answer without ever asking the new one.
    store.client = client;
    store.inFlight = null;
    store.dirty = false;
    publish([]);
  }
  if (store.inFlight !== null) {
    store.dirty = true;
    return store.inFlight;
  }
  const running = poll(client).finally(() => {
    store.inFlight = null;
    if (store.dirty) {
      store.dirty = false;
      void refresh(client);
    }
  });
  store.inFlight = running;
  return running;
}

/// Ask the server again, now. Call it from anything that discharges a
/// signal the badge counts — reading a card, a board change arriving over
/// the socket, answering an approval.
export function invalidateAttention(client: AdminClient): void {
  void refresh(client);
}

export function useAttention(): ProjectAttention[] {
  const client = useAdminClient();
  // Seeded from the cache so a component that mounts and unmounts with a
  // dropdown does not flash its mark off and on — but only from a cache
  // this client filled.
  const [boards, setBoards] = useState<ProjectAttention[]>(() =>
    store.client === client ? store.boards : [],
  );

  useEffect(() => {
    const listener = () => {
      setBoards(store.boards);
    };
    store.listeners.add(listener);
    // The timer belongs to the store, not to a component: the rail is
    // mounted on every authed route and never unmounts, so a per-component
    // timer would be indistinguishable from this until a second component
    // mounted and started a second one.
    if (store.timer === null) {
      store.timer = setInterval(() => {
        void refresh(client);
      }, POLL_MS);
    }
    void refresh(client);
    return () => {
      store.listeners.delete(listener);
      if (store.listeners.size === 0 && store.timer !== null) {
        clearInterval(store.timer);
        store.timer = null;
      }
    };
  }, [client]);

  return boards;
}

/// Whether anything at all is waiting on the operator. A dot, not a count:
/// the rail entry opens exactly one board, so a total across boards is a
/// number clicking it cannot discharge — and the per-card badges on the
/// board itself are where a number means something you can act on.
export function needsAttention(boards: ProjectAttention[]): boolean {
  return boards.length > 0;
}

export function attentionFor(
  boards: ProjectAttention[],
  projectId: string,
): ProjectAttention | null {
  return boards.find((board) => board.project_id === projectId) ?? null;
}

/// Whether some board *other* than this one is waiting. What the switcher's
/// trigger needs: the rail's dot says something is lit somewhere, and
/// without this the operator has no way to tell that the somewhere is
/// behind the dropdown rather than on the board in front of them.
export function attentionElsewhere(
  boards: ProjectAttention[],
  projectId: string | null,
): boolean {
  return boards.some((board) => board.project_id !== projectId);
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
