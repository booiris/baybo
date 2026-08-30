/**
 * Indexes the subagent sessions descended from the session being viewed, so
 * the tree can place a jump to each child's own trace under the
 * `spawn_subagent` span that started it.
 *
 * Pure — the shape of the forest and its cross-boundary failure roll-up are
 * decided here, not while rendering.
 */
import type { LineageSession, TraceTurnSummary, TurnStatusKind, TurnTrace } from '../../types/trace';
import { failureCount, turnFailed } from './traceTreeModel';

export interface TraceForest {
  /** Children indexed by the parent span they hang under. */
  bySpan: Map<string, LineageSession[]>;
  /**
   * Children whose `parent_span_id` was never recorded, indexed by parent
   * turn. They still belong in the tree — attached at the turn rather than at
   * a specific span — so an older trace degrades to a slightly coarser
   * position rather than hiding its subagents entirely.
   */
  byTurn: Map<string, LineageSession[]>;
  /** Every descendant, by session id. */
  byId: Map<string, LineageSession>;
  /** Descendants of a given session, in spawn order. */
  byParentSession: Map<string, LineageSession[]>;
}

function push<T>(map: Map<string, T[]>, key: string, value: T): void {
  const bucket = map.get(key);
  if (bucket) bucket.push(value);
  else map.set(key, [value]);
}

export function buildForest(sessions: LineageSession[]): TraceForest {
  const forest: TraceForest = {
    bySpan: new Map(),
    byTurn: new Map(),
    byId: new Map(),
    byParentSession: new Map(),
  };
  for (const s of sessions) {
    forest.byId.set(s.session_id, s);
    push(forest.byParentSession, s.parent_session_id, s);
    if (s.parent_span_id != null && s.parent_span_id !== '') push(forest.bySpan, s.parent_span_id, s);
    else push(forest.byTurn, s.parent_turn_id, s);
  }
  return forest;
}

/**
 * Whether a child session's subtree holds a failure — its own turns, plus
 * every session descended from it. This is what lets the `spawn_subagent`
 * jump say "something failed in there" before the reader opens the child.
 *
 * Turn *status* is the signal, not a loaded step tree: the lineage response
 * carries statuses for every descendant up front, while step trees are
 * not fetched on the parent page, so a status-based roll-up is the only one
 * that can cross the boundary. Where a turn's tree happens to be loaded, a
 * failing span counts too — a turn can report `completed` while a span inside
 * it failed and was recovered.
 */
export function sessionHasFailure(
  forest: TraceForest,
  sessionId: string,
  turnTraces: Map<string, TurnTrace>,
  seen: Set<string> = new Set(),
): boolean {
  if (seen.has(sessionId)) return false;
  seen.add(sessionId);
  const child = forest.byId.get(sessionId);
  if (child && turnsHaveFailure(child.turns, turnTraces)) return true;
  for (const grandchild of forest.byParentSession.get(sessionId) ?? []) {
    if (sessionHasFailure(forest, grandchild.session_id, turnTraces, seen)) return true;
  }
  return false;
}

function turnsHaveFailure(turns: TraceTurnSummary[], turnTraces: Map<string, TurnTrace>): boolean {
  return turns.some((t) => {
    if (turnFailed(t.turn_status_kind)) return true;
    const trace = turnTraces.get(t.turn_id);
    return trace ? failureCount(trace) > 0 : false;
  });
}

/** Whether any turn in a child's subtree is still running — drives the live
 *  spinner on its jump row. */
export function sessionIsLive(
  forest: TraceForest,
  sessionId: string,
  seen: Set<string> = new Set(),
): boolean {
  if (seen.has(sessionId)) return false;
  seen.add(sessionId);
  const child = forest.byId.get(sessionId);
  if (child != null && child.turns.some((t) => turnRunning(t.turn_status_kind))) return true;
  for (const grandchild of forest.byParentSession.get(sessionId) ?? []) {
    if (sessionIsLive(forest, grandchild.session_id, seen)) return true;
  }
  return false;
}

/** Every descendant session that has at least one running turn. The parent
 *  page stays on its slow poll tier while this is non-empty so jump status can
 *  settle without a manual refresh. */
export function liveSessions(forest: TraceForest): string[] {
  return [...forest.byId.values()]
    .filter((child) => child.turns.some((t) => turnRunning(t.turn_status_kind)))
    .map((child) => child.session_id);
}

/** A turn that is still doing work. Narrower than `isTurnLive`, which also
 *  counts `stuck` — a stuck subagent cannot change without outside action, so
 *  it must not keep the parent page polling forever. */
function turnRunning(status: TurnStatusKind): boolean {
  return status === 'pending' || status === 'in_progress';
}
