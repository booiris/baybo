import { describe, expect, it } from 'vitest';
import type { JobTrace, LifecycleState, ReplayStep, Span, Step, TraceJobSummary } from '../../types/trace';
import {
  attention,
  failureCount,
  findSpan,
  findStep,
  isExternalAgentJob,
  isJobLive,
  jobFailed,
  jobRollup,
  neededJobIds,
  resolveExpanded,
  traceHasPendingSpan,
} from './traceTreeModel';

const T0 = '2026-01-01T00:00:00.000Z';
const T1 = '2026-01-01T00:00:01.000Z';

function mkSpan(id: string, stepId: string, outcome: LifecycleState): Span {
  return {
    id,
    step_id: stepId,
    kind: {
      kind: 'llm_call',
      begin: { model_id: 'm', provider: 'p', provider_config_hash: 'h', input_messages: [] },
      result: null,
    },
    parallel_group: null,
    started_at: T0,
    ended_at: outcome.outcome === 'pending' ? null : T1,
    outcome,
  };
}

function mkStep(id: string, outcome: LifecycleState, spans: Span[]): ReplayStep {
  const step: Step = {
    id,
    job_id: 'job',
    kind: { kind: 'llm_iteration' },
    started_at: T0,
    ended_at: outcome.outcome === 'pending' ? null : T1,
    outcome,
  };
  return { step, spans };
}

function mkTrace(jobId: string, steps: ReplayStep[]): JobTrace {
  return { job_id: jobId, session_id: 's', job_status_kind: 'completed', steps };
}

function mkJob(jobId: string, status: TraceJobSummary['job_status_kind']): TraceJobSummary {
  return {
    job_id: jobId,
    session_id: 's',
    job_status_kind: status,
    created_at: T0,
    started_at: T0,
    ended_at: T1,
    input_tokens: 0,
    output_tokens: 0,
    cached_input_tokens: 0,
    cache_creation_input_tokens: 0,
  };
}

const ok: LifecycleState = { outcome: 'ok' };
const failed: LifecycleState = { outcome: 'failed', reason: 'boom' };
const pending: LifecycleState = { outcome: 'pending' };
const cancelled: LifecycleState = { outcome: 'cancelled', reason: 'user_stopped' };

describe('attention / jobFailed / isJobLive', () => {
  it('attention is true for anything but ok', () => {
    expect(attention(ok)).toBe(false);
    expect(attention(failed)).toBe(true);
    expect(attention(pending)).toBe(true);
    expect(attention(cancelled)).toBe(true);
  });

  it('jobFailed covers failed and stuck', () => {
    expect(jobFailed('failed')).toBe(true);
    expect(jobFailed('stuck')).toBe(true);
    expect(jobFailed('completed')).toBe(false);
    expect(jobFailed('in_progress')).toBe(false);
  });

  it('isJobLive covers pending, in_progress, stuck', () => {
    expect(isJobLive('pending')).toBe(true);
    expect(isJobLive('in_progress')).toBe(true);
    expect(isJobLive('stuck')).toBe(true);
    expect(isJobLive('completed')).toBe(false);
    expect(isJobLive('failed')).toBe(false);
  });
});

describe('failureCount', () => {
  it('counts failed/cancelled spans', () => {
    const trace = mkTrace('j', [
      mkStep('s1', ok, [mkSpan('a', 's1', ok), mkSpan('b', 's1', failed)]),
      mkStep('s2', ok, [mkSpan('c', 's2', cancelled)]),
    ]);
    expect(failureCount(trace)).toBe(2);
  });

  it('counts a failed span-less step once', () => {
    const trace = mkTrace('j', [mkStep('s1', failed, [])]);
    expect(failureCount(trace)).toBe(1);
  });

  it('does not double-count a failed step that already has a failed span', () => {
    const trace = mkTrace('j', [mkStep('s1', failed, [mkSpan('a', 's1', failed)])]);
    expect(failureCount(trace)).toBe(1);
  });

  it('is zero for an all-ok trace', () => {
    const trace = mkTrace('j', [mkStep('s1', ok, [mkSpan('a', 's1', ok)])]);
    expect(failureCount(trace)).toBe(0);
  });
});

describe('jobRollup', () => {
  it('uses the loaded trace for a precise count', () => {
    const trace = mkTrace('j', [mkStep('s1', ok, [mkSpan('a', 's1', failed)])]);
    expect(jobRollup(mkJob('j', 'completed'), trace)).toEqual({ hasFailure: true, count: 1 });
  });

  it('flags a failed span inside a completed job once loaded', () => {
    // The cheap status approximation would say completed=clean; the loaded
    // trace is authoritative.
    const trace = mkTrace('j', [mkStep('s1', ok, [mkSpan('a', 's1', failed)])]);
    expect(jobRollup(mkJob('j', 'completed'), trace).hasFailure).toBe(true);
  });

  it('falls back to status when the trace is not loaded', () => {
    expect(jobRollup(mkJob('j', 'failed'), undefined)).toEqual({ hasFailure: true, count: null });
    expect(jobRollup(mkJob('j', 'completed'), undefined)).toEqual({ hasFailure: false, count: null });
  });

  it('keeps the badge for a stuck/failed job whose spans are all ok (job-level failure)', () => {
    const clean = mkTrace('j', [mkStep('s1', ok, [mkSpan('a', 's1', ok)])]);
    expect(jobRollup(mkJob('j', 'stuck'), clean)).toEqual({ hasFailure: true, count: null });
    expect(jobRollup(mkJob('j', 'failed'), clean)).toEqual({ hasFailure: true, count: null });
  });
});

describe('resolveExpanded', () => {
  it('honors an explicit user override over the default', () => {
    const toggles = new Map<string, boolean>([['x', false]]);
    expect(resolveExpanded('x', toggles, true)).toBe(false);
    expect(resolveExpanded('y', toggles, true)).toBe(true);
    expect(resolveExpanded('y', toggles, false)).toBe(false);
  });
});

describe('neededJobIds', () => {
  const jobs = [mkJob('a', 'completed'), mkJob('b', 'failed'), mkJob('c', 'completed')];

  it('includes every job by default (all expanded)', () => {
    expect(neededJobIds(jobs, new Map(), null).sort()).toEqual(['a', 'b', 'c']);
  });

  it('excludes a job the user explicitly collapsed', () => {
    const toggles = new Map<string, boolean>([['b', false]]);
    expect(neededJobIds(jobs, toggles, null).sort()).toEqual(['a', 'c']);
  });

  it('always includes the selected job, even if collapsed', () => {
    const toggles = new Map<string, boolean>([['a', false]]);
    expect(neededJobIds(jobs, toggles, 'a').sort()).toEqual(['a', 'b', 'c']);
  });
});

describe('findSpan / findStep', () => {
  const trace = mkTrace('j', [
    mkStep('s1', ok, [mkSpan('a', 's1', ok)]),
    mkStep('s2', ok, [mkSpan('b', 's2', ok)]),
  ]);

  it('locates a span and its owning step', () => {
    expect(findSpan(trace, 'b')?.stepId).toBe('s2');
    expect(findSpan(trace, 'missing')).toBeNull();
    expect(findSpan(undefined, 'a')).toBeNull();
  });

  it('locates a step', () => {
    expect(findStep(trace, 's1')?.step.id).toBe('s1');
    expect(findStep(trace, 'missing')).toBeNull();
    expect(findStep(undefined, 's1')).toBeNull();
  });
});

describe('isExternalAgentJob / traceHasPendingSpan', () => {
  it('flags a terminal loaded-but-empty trace as an external agent', () => {
    expect(isExternalAgentJob(mkTrace('j', []), 'completed')).toBe(true);
    expect(isExternalAgentJob(mkTrace('j', []), 'failed')).toBe(true);
    expect(isExternalAgentJob(mkTrace('j', [mkStep('s1', ok, [])]), 'completed')).toBe(false);
    // A not-yet-loaded job must not be mistaken for an external agent.
    expect(isExternalAgentJob(undefined, 'completed')).toBe(false);
  });

  it('does NOT flag a live empty job as external (steps may not have flushed yet)', () => {
    expect(isExternalAgentJob(mkTrace('j', []), 'in_progress')).toBe(false);
    expect(isExternalAgentJob(mkTrace('j', []), 'pending')).toBe(false);
    expect(isExternalAgentJob(mkTrace('j', []), 'stuck')).toBe(false);
  });

  it('detects a pending span or step', () => {
    expect(traceHasPendingSpan(mkTrace('j', [mkStep('s1', ok, [mkSpan('a', 's1', pending)])]))).toBe(true);
    expect(traceHasPendingSpan(mkTrace('j', [mkStep('s1', pending, [])]))).toBe(true);
    expect(traceHasPendingSpan(mkTrace('j', [mkStep('s1', ok, [mkSpan('a', 's1', ok)])]))).toBe(false);
    expect(traceHasPendingSpan(undefined)).toBe(false);
  });
});
