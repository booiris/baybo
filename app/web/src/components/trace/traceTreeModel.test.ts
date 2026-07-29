import { describe, expect, it } from 'vitest';
import type { LifecycleState, ReplayStep, Span, Step, TraceTurnSummary, TurnInputKind, TurnTrace } from '../../types/trace';
import {
  attention,
  failureCount,
  findSpan,
  findStep,
  isExternalAgentTurn,
  isTurnLive,
  neededTurnIds,
  resolveExpanded,
  traceHasPendingSpan,
  turnLabels,
  turnFailed,
  turnRollup,
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
    turn_id: 'turn',
    kind: { kind: 'llm_iteration' },
    started_at: T0,
    ended_at: outcome.outcome === 'pending' ? null : T1,
    outcome,
  };
  return { step, spans };
}

function mkTrace(turnId: string, steps: ReplayStep[]): TurnTrace {
  return { turn_id: turnId, session_id: 's', turn_status_kind: 'completed', turn_input_kind: 'user_chat', steps };
}

function mkKindTurn(turnId: string, kind: TurnInputKind): TraceTurnSummary {
  return { ...mkTurn(turnId, 'completed'), turn_input_kind: kind };
}

function mkTurn(turnId: string, status: TraceTurnSummary['turn_status_kind']): TraceTurnSummary {
  return {
    turn_id: turnId,
    session_id: 's',
    turn_status_kind: status,
    turn_input_kind: 'user_chat',
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

describe('attention / turnFailed / isTurnLive', () => {
  it('attention is true for anything but ok', () => {
    expect(attention(ok)).toBe(false);
    expect(attention(failed)).toBe(true);
    expect(attention(pending)).toBe(true);
    expect(attention(cancelled)).toBe(true);
  });

  it('turnFailed covers failed and stuck', () => {
    expect(turnFailed('failed')).toBe(true);
    expect(turnFailed('stuck')).toBe(true);
    expect(turnFailed('completed')).toBe(false);
    expect(turnFailed('in_progress')).toBe(false);
  });

  it('isTurnLive covers pending, in_progress, stuck', () => {
    expect(isTurnLive('pending')).toBe(true);
    expect(isTurnLive('in_progress')).toBe(true);
    expect(isTurnLive('stuck')).toBe(true);
    expect(isTurnLive('completed')).toBe(false);
    expect(isTurnLive('failed')).toBe(false);
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

describe('turnRollup', () => {
  it('uses the loaded trace for a precise count', () => {
    const trace = mkTrace('j', [mkStep('s1', ok, [mkSpan('a', 's1', failed)])]);
    expect(turnRollup(mkTurn('j', 'completed'), trace)).toEqual({ hasFailure: true, count: 1 });
  });

  it('flags a failed span inside a completed turn once loaded', () => {
    // The cheap status approximation would say completed=clean; the loaded
    // trace is authoritative.
    const trace = mkTrace('j', [mkStep('s1', ok, [mkSpan('a', 's1', failed)])]);
    expect(turnRollup(mkTurn('j', 'completed'), trace).hasFailure).toBe(true);
  });

  it('falls back to status when the trace is not loaded', () => {
    expect(turnRollup(mkTurn('j', 'failed'), undefined)).toEqual({ hasFailure: true, count: null });
    expect(turnRollup(mkTurn('j', 'completed'), undefined)).toEqual({ hasFailure: false, count: null });
  });

  it('keeps the badge for a stuck/failed turn whose spans are all ok (turn-level failure)', () => {
    const clean = mkTrace('j', [mkStep('s1', ok, [mkSpan('a', 's1', ok)])]);
    expect(turnRollup(mkTurn('j', 'stuck'), clean)).toEqual({ hasFailure: true, count: null });
    expect(turnRollup(mkTurn('j', 'failed'), clean)).toEqual({ hasFailure: true, count: null });
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

describe('neededTurnIds', () => {
  const turns = [mkTurn('a', 'completed'), mkTurn('b', 'failed'), mkTurn('c', 'completed')];

  it('includes every turn by default (all expanded)', () => {
    expect(neededTurnIds(turns, new Map(), null).sort()).toEqual(['a', 'b', 'c']);
  });

  it('excludes a turn the user explicitly collapsed', () => {
    const toggles = new Map<string, boolean>([['b', false]]);
    expect(neededTurnIds(turns, toggles, null).sort()).toEqual(['a', 'c']);
  });

  it('always includes the selected turn, even if collapsed', () => {
    const toggles = new Map<string, boolean>([['a', false]]);
    expect(neededTurnIds(turns, toggles, 'a').sort()).toEqual(['a', 'b', 'c']);
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

describe('isExternalAgentTurn / traceHasPendingSpan', () => {
  it('flags a terminal loaded-but-empty trace as an external agent', () => {
    expect(isExternalAgentTurn(mkTrace('j', []), 'completed')).toBe(true);
    expect(isExternalAgentTurn(mkTrace('j', []), 'failed')).toBe(true);
    expect(isExternalAgentTurn(mkTrace('j', [mkStep('s1', ok, [])]), 'completed')).toBe(false);
    // A not-yet-loaded turn must not be mistaken for an external agent.
    expect(isExternalAgentTurn(undefined, 'completed')).toBe(false);
  });

  it('does NOT flag a live empty turn as external (steps may not have flushed yet)', () => {
    expect(isExternalAgentTurn(mkTrace('j', []), 'in_progress')).toBe(false);
    expect(isExternalAgentTurn(mkTrace('j', []), 'pending')).toBe(false);
    expect(isExternalAgentTurn(mkTrace('j', []), 'stuck')).toBe(false);
  });

  it('detects a pending span or step', () => {
    expect(traceHasPendingSpan(mkTrace('j', [mkStep('s1', ok, [mkSpan('a', 's1', pending)])]))).toBe(true);
    expect(traceHasPendingSpan(mkTrace('j', [mkStep('s1', pending, [])]))).toBe(true);
    expect(traceHasPendingSpan(mkTrace('j', [mkStep('s1', ok, [mkSpan('a', 's1', ok)])]))).toBe(false);
    expect(traceHasPendingSpan(undefined)).toBe(false);
  });
});

describe('turnLabels', () => {
  it('numbers only the turns the chat transcript showed', () => {
    // Two user messages with a compaction between them: the chat rendered two
    // turns, so the viewer must too — numbering the compaction as #2 is the
    // disagreement this labelling exists to prevent.
    const turns = [
      mkKindTurn('a', 'user_chat'),
      mkKindTurn('b', 'compact'),
      mkKindTurn('c', 'user_chat'),
    ];
    expect(turnLabels(turns).map((l) => l.long)).toEqual(['Turn #1', 'Compaction', 'Turn #2']);
    expect(turnLabels(turns).map((l) => l.short)).toEqual(['#1', 'cmp', '#2']);
  });

  it('treats a cron-result delivery as non-chat, and a cron fire as a turn', () => {
    const turns = [
      mkKindTurn('a', 'cron'),
      mkKindTurn('b', 'cron_notification'),
      mkKindTurn('c', 'spawned'),
      mkKindTurn('d', 'subagent_notification'),
    ];
    expect(turnLabels(turns).map((l) => l.long)).toEqual([
      'Turn #1',
      'Cron delivery',
      'Turn #2',
      'Turn #3',
    ]);
  });

  it('is empty-safe and numbers a pure-maintenance session with no turns at all', () => {
    expect(turnLabels([])).toEqual([]);
    expect(turnLabels([mkKindTurn('a', 'compact')]).map((l) => l.long)).toEqual(['Compaction']);
  });
});
