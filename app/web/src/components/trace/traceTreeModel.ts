/**
 * Pure logic behind the trace tree: which nodes carry a failure, which are
 * expanded by default (the failure path + the current selection), which jobs'
 * step trees must be fetched, and span/step lookups. No React, no rendering —
 * so it stays unit-testable and the `TraceTree` component reads as layout.
 */
import type { JobStatusKind, JobTrace, LifecycleState, ReplayStep, Span, TraceJobSummary } from '../../types/trace';

/**
 * A node "needs attention" when its outcome is anything other than `ok` —
 * `failed` / `cancelled` (terminal problems) or `pending` (still in flight).
 * These are the nodes the tree auto-expands and badges.
 */
export function attention(state: LifecycleState): boolean {
  return state.outcome !== 'ok';
}

export function jobFailed(status: JobStatusKind): boolean {
  return status === 'failed' || status === 'stuck';
}

export function isJobLive(status: JobStatusKind): boolean {
  return status === 'pending' || status === 'in_progress' || status === 'stuck';
}

export function traceHasPendingSpan(trace: JobTrace | undefined): boolean {
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
 * precise roll-up count on a job/step row once its tree is loaded.
 */
export function failureCount(trace: JobTrace): number {
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

export interface JobRollup {
  /** Whether this job's subtree contains (or is presumed to contain) a failure. */
  hasFailure: boolean;
  /** Precise failing-leaf count once the trace is loaded; `null` when unknown. */
  count: number | null;
}

/**
 * Roll-up badge state for a job row. With the trace loaded we count failing
 * leaves exactly, but still honor `job_status_kind` — a `stuck`/`failed` job
 * whose recorded spans are all `ok` (the failure is the missing next step, or
 * recorded at the job level) must keep its badge and survive the "failures
 * only" filter. Without a trace we fall back to status alone — a cheap
 * approximation (see PR1 deferred notes in docs/todo/trace-tree-redesign.md).
 */
export function jobRollup(summary: TraceJobSummary, trace: JobTrace | undefined): JobRollup {
  const statusFail = jobFailed(summary.job_status_kind);
  if (trace) {
    const count = failureCount(trace);
    return { hasFailure: count > 0 || statusFail, count: count > 0 ? count : null };
  }
  return { hasFailure: statusFail, count: null };
}

/** Resolve a node's expanded state: an explicit user toggle wins over the
 *  default. Jobs and steps are expanded by default (nothing is hidden — the
 *  bench-web "show the whole flow" model), so the default is `true`; a chevron
 *  click records `false` to collapse a specific node. */
export function resolveExpanded(id: string, userToggles: Map<string, boolean>, def: boolean): boolean {
  const override = userToggles.get(id);
  return override === undefined ? def : override;
}

/**
 * The set of job ids whose step tree must be fetched. Since every job is
 * expanded by default (nothing collapsed), that's every job — except the ones
 * the user explicitly collapsed (a collapsed job needs no tree). The selected
 * job is always included.
 */
export function neededJobIds(
  jobs: TraceJobSummary[],
  userToggles: Map<string, boolean>,
  selectedJobId: string | null,
): string[] {
  const out: string[] = [];
  for (const j of jobs) {
    if (j.job_id === selectedJobId || resolveExpanded(j.job_id, userToggles, true)) {
      out.push(j.job_id);
    }
  }
  return out;
}

/** Locate a span (and its step) inside a loaded job trace. */
export function findSpan(trace: JobTrace | undefined, spanId: string): { span: Span; stepId: string } | null {
  if (!trace) return null;
  for (const rs of trace.steps) {
    const span = rs.spans.find((s) => s.id === spanId);
    if (span) return { span, stepId: rs.step.id };
  }
  return null;
}

export function findStep(trace: JobTrace | undefined, stepId: string): ReplayStep | null {
  if (!trace) return null;
  return trace.steps.find((rs) => rs.step.id === stepId) ?? null;
}

/**
 * A job with a fetched-but-empty step tree, once it is no longer live, is an
 * external agent (claude/codex) whose loop is opaque — render its transcript
 * instead of a step tree. The `!isJobLive` gate is essential: a pending /
 * in_progress internal job can momentarily have zero recorded steps, and must
 * NOT be mislabeled an external agent (it self-corrects as steps land).
 */
export function isExternalAgentJob(trace: JobTrace | undefined, status: JobStatusKind): boolean {
  return !!trace && trace.steps.length === 0 && !isJobLive(status);
}
