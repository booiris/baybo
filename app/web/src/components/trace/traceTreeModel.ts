/**
 * Pure logic behind the trace tree: which nodes carry a failure, which are
 * expanded by default (the failure path + the current selection), which turns'
 * step trees must be fetched, and span/step lookups. No React, no rendering —
 * so it stays unit-testable and the `TraceTree` component reads as layout.
 */
import type { LifecycleState, ReplayStep, Span, TraceTurnSummary, TurnStatusKind, TurnTrace } from '../../types/trace';

/**
 * A node "needs attention" when its outcome is anything other than `ok` —
 * `failed` / `cancelled` (terminal problems) or `pending` (still in flight).
 * These are the nodes the tree auto-expands and badges.
 */
export function attention(state: LifecycleState): boolean {
  return state.outcome !== 'ok';
}

export function turnFailed(status: TurnStatusKind): boolean {
  return status === 'failed' || status === 'stuck';
}

export function isTurnLive(status: TurnStatusKind): boolean {
  return status === 'pending' || status === 'in_progress' || status === 'stuck';
}

export function traceHasPendingSpan(trace: TurnTrace | undefined): boolean {
  if (!trace) return false;
  for (const rs of trace.steps) {
    if (rs.step.outcome.outcome === 'pending') return true;
    for (const s of rs.spans) {
      if (s.outcome.outcome === 'pending') return true;
    }
  }
  return false;
}

/**
 * Count the failing leaves in a loaded trace — spans whose outcome is
 * `failed`/`cancelled`, plus any span-less step that itself failed. Drives the
 * precise roll-up count on a turn/step row once its tree is loaded.
 */
export function failureCount(trace: TurnTrace): number {
  let n = 0;
  for (const rs of trace.steps) {
    let stepSpanFailures = 0;
    for (const s of rs.spans) {
      if (s.outcome.outcome === 'failed' || s.outcome.outcome === 'cancelled') {
        n += 1;
        stepSpanFailures += 1;
      }
    }
    if (
      stepSpanFailures === 0 &&
      (rs.step.outcome.outcome === 'failed' || rs.step.outcome.outcome === 'cancelled')
    ) {
      n += 1;
    }
  }
  return n;
}

export interface TurnRollup {
  /** Whether this turn's subtree contains (or is presumed to contain) a failure. */
  hasFailure: boolean;
  /** Precise failing-leaf count once the trace is loaded; `null` when unknown. */
  count: number | null;
}

/**
 * Roll-up badge state for a turn row. With the trace loaded we count failing
 * leaves exactly, but still honor `turn_status_kind` — a `stuck`/`failed` turn
 * whose recorded spans are all `ok` (the failure is the missing next step, or
 * recorded at the turn level) must keep its badge and survive the "failures
 * only" filter. Without a trace we fall back to status alone — a cheap
 * approximation (see PR1 deferred notes in docs/todo/trace-tree-redesign.md).
 */
export function turnRollup(summary: TraceTurnSummary, trace: TurnTrace | undefined): TurnRollup {
  const statusFail = turnFailed(summary.turn_status_kind);
  if (trace) {
    const count = failureCount(trace);
    return { hasFailure: count > 0 || statusFail, count: count > 0 ? count : null };
  }
  return { hasFailure: statusFail, count: null };
}

/** Resolve a node's expanded state: an explicit user toggle wins over the
 *  default. Turns and steps are expanded by default (nothing is hidden — the
 *  bench-web "show the whole flow" model), so the default is `true`; a chevron
 *  click records `false` to collapse a specific node. */
export function resolveExpanded(id: string, userToggles: Map<string, boolean>, def: boolean): boolean {
  const override = userToggles.get(id);
  return override === undefined ? def : override;
}

/**
 * The set of turn ids whose step tree must be fetched. Since every turn is
 * expanded by default (nothing collapsed), that's every turn — except the ones
 * the user explicitly collapsed (a collapsed turn needs no tree). The selected
 * turn is always included.
 */
export function neededTurnIds(
  turns: TraceTurnSummary[],
  userToggles: Map<string, boolean>,
  selectedTurnId: string | null,
): string[] {
  const out: string[] = [];
  for (const j of turns) {
    if (j.turn_id === selectedTurnId || resolveExpanded(j.turn_id, userToggles, true)) {
      out.push(j.turn_id);
    }
  }
  return out;
}

/** Locate a span (and its step) inside a loaded turn trace. */
export function findSpan(trace: TurnTrace | undefined, spanId: string): { span: Span; stepId: string } | null {
  if (!trace) return null;
  for (const rs of trace.steps) {
    const span = rs.spans.find((s) => s.id === spanId);
    if (span) return { span, stepId: rs.step.id };
  }
  return null;
}

export function findStep(trace: TurnTrace | undefined, stepId: string): ReplayStep | null {
  if (!trace) return null;
  return trace.steps.find((rs) => rs.step.id === stepId) ?? null;
}

/**
 * A turn with a fetched-but-empty step tree, once it is no longer live, is an
 * external agent (claude/codex) whose loop is opaque — render its transcript
 * instead of a step tree. The `!isTurnLive` gate is essential: a pending /
 * in_progress internal turn can momentarily have zero recorded steps, and must
 * NOT be mislabeled an external agent (it self-corrects as steps land).
 */
export function isExternalAgentTurn(trace: TurnTrace | undefined, status: TurnStatusKind): boolean {
  return !!trace && trace.steps.length === 0 && !isTurnLive(status);
}
