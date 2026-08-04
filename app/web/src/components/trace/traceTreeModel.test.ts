import { describe, expect, it } from 'vitest';
import type {
  LifecycleState,
  ReplayStep,
  SessionMessageRow,
  Span,
  Step,
  TraceTurnSummary,
  TurnInputKind,
  TurnTrace,
} from '../../types/trace';
import {
  attention,
  failureCount,
  findSpan,
  findStep,
  isExternalAgentTurn,
  isTurnLive,
  neededTurnIds,
  partitionTranscript,
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

// ── External agents: the marker, and the transcript-as-trace partition ──
//
// Source of truth: `TraceOverview.external_agent` (see the doc comment on
// `crates/gateway/src/api/admin/traces.rs`) — a session whose work ran on a
// claude/codex/gemini binary records NO step/span tree, ever, so its
// `session_messages` transcript IS its trace and the middle pane renders that
// instead of a step tree.

/** A turn whose window boundary is explicit. `created_at` defaults to the
 *  start, so the two only diverge where a test means them to. */
function mkTurnAt(turnId: string, startedAt: string, createdAt: string = startedAt): TraceTurnSummary {
  return { ...mkTurn(turnId, 'completed'), created_at: createdAt, started_at: startedAt };
}

function mkRow(ordinal: number, createdAt: string, supersededBy: number | null = null): SessionMessageRow {
  return {
    ordinal,
    superseded_by: supersededBy,
    created_at: createdAt,
    message: { role: 'user', source: 'user', content: [{ Text: `row ${ordinal}` }] },
  };
}

/** Buckets flattened to `{ turn_id: [ordinal, …] }`; order inside a bucket is
 *  the order the rows were fed in. */
function byTurn(map: Map<string, SessionMessageRow[]>): Record<string, number[]> {
  return Object.fromEntries([...map].map(([id, rows]) => [id, rows.map((r) => r.ordinal)]));
}

const S0 = '2026-02-01T00:00:00.000Z'; // turn `a` starts
const S1 = '2026-02-01T00:10:00.000Z'; // turn `b` starts
const S2 = '2026-02-01T00:20:00.000Z'; // turn `c` starts
const BEFORE_S0 = '2026-01-31T23:59:59.000Z';
const MID_A = '2026-02-01T00:05:00.000Z';
const MID_B = '2026-02-01T00:15:00.000Z';
const AFTER_S2 = '2026-02-01T09:00:00.000Z';

describe('isExternalAgentTurn (session-level external_agent marker)', () => {
  const empty = mkTrace('j', []);
  const withSteps = mkTrace('j', [mkStep('s1', ok, [])]);

  it('flags a LIVE zero-step turn — the whole point of the marker', () => {
    // The steps are never coming, so gating on a terminal status gates forever:
    // without this a running claude/codex subagent renders nothing at all.
    expect(isExternalAgentTurn(empty, 'in_progress', 'claude')).toBe(true);
    expect(isExternalAgentTurn(empty, 'pending', 'claude')).toBe(true);
    expect(isExternalAgentTurn(empty, 'stuck', 'claude')).toBe(true);
  });

  it('keeps flagging terminal zero-step turns, for every backend', () => {
    expect(isExternalAgentTurn(empty, 'completed', 'codex')).toBe(true);
    expect(isExternalAgentTurn(empty, 'failed', 'gemini')).toBe(true);
    expect(isExternalAgentTurn(empty, 'cancelled', 'claude')).toBe(true);
  });

  it('never mislabels a real step tree — recorded steps win over the marker', () => {
    expect(isExternalAgentTurn(withSteps, 'in_progress', 'claude')).toBe(false);
    expect(isExternalAgentTurn(withSteps, 'completed', 'claude')).toBe(false);
  });

  it('counts an unfetched trace as external WHEN marked', () => {
    // The page never fetches turn trees for a marked external session — there
    // is no tree to fetch — so `undefined` is the steady state, not a
    // not-yet-loaded state. Requiring a fetched trace here would mean the
    // transcript never rendered at all for the sessions this marker exists for.
    expect(isExternalAgentTurn(undefined, 'in_progress', 'claude')).toBe(true);
    expect(isExternalAgentTurn(undefined, 'completed', 'claude')).toBe(true);
    expect(isExternalAgentTurn(undefined, 'stuck', 'codex')).toBe(true);
  });

  it('an unfetched trace is never external WITHOUT a marker', () => {
    // Unmarked, an absent tree says nothing — it may simply not have loaded.
    expect(isExternalAgentTurn(undefined, 'in_progress', null)).toBe(false);
    expect(isExternalAgentTurn(undefined, 'completed', null)).toBe(false);
    expect(isExternalAgentTurn(undefined, 'completed', undefined)).toBe(false);
  });

  it('leaves the no-marker heuristic exactly as it was', () => {
    // Sessions written before the backend tag reached the wire: a live internal
    // turn can momentarily have zero steps and must not be relabelled.
    expect(isExternalAgentTurn(empty, 'in_progress', null)).toBe(false);
    expect(isExternalAgentTurn(empty, 'in_progress', undefined)).toBe(false);
    expect(isExternalAgentTurn(empty, 'completed', null)).toBe(true);
    expect(isExternalAgentTurn(empty, 'completed', undefined)).toBe(true);
    // …and omitting the argument is the same as passing undefined.
    expect(isExternalAgentTurn(empty, 'in_progress')).toBe(false);
    expect(isExternalAgentTurn(empty, 'completed')).toBe(true);
  });
});

describe('partitionTranscript', () => {
  const turns = [mkTurnAt('a', S0), mkTurnAt('b', S1), mkTurnAt('c', S2)];

  it('buckets each row to the last turn that had started when it was written', () => {
    const rows = [mkRow(1, S0), mkRow(2, MID_A), mkRow(3, S1), mkRow(4, MID_B), mkRow(5, S2)];
    expect(byTurn(partitionTranscript(rows, turns))).toEqual({ a: [1, 2], b: [3, 4], c: [5] });
  });

  it('gives a row written exactly on a boundary to the turn that just opened', () => {
    expect(byTurn(partitionTranscript([mkRow(1, S1)], turns))).toEqual({ b: [1] });
  });

  it('folds rows that predate the first turn into it rather than dropping them', () => {
    // An external run persists its task prompt around the same instant the turn
    // opens, and the two orderings are not guaranteed.
    const rows = [mkRow(1, BEFORE_S0), mkRow(2, S0)];
    expect(byTurn(partitionTranscript(rows, turns))).toEqual({ a: [1, 2] });
  });

  it("leaves the newest turn's window open-ended so a live run's rows land", () => {
    const live = [mkTurnAt('a', S0), mkTurnAt('b', S1)];
    const rows = [mkRow(1, S1), mkRow(2, MID_B), mkRow(3, AFTER_S2)];
    expect(byTurn(partitionTranscript(rows, live))).toEqual({ b: [1, 2, 3] });
  });

  it('drops superseded rows entirely — a compaction rewrote them', () => {
    const rows = [mkRow(1, S0), mkRow(2, MID_A, 42), mkRow(3, S1), mkRow(4, MID_B, 42)];
    expect(byTurn(partitionTranscript(rows, turns))).toEqual({ a: [1], b: [3] });
  });

  it('treats a 0 supersede marker as superseded (a marker, not a falsy no-op)', () => {
    expect(byTurn(partitionTranscript([mkRow(1, S0, 0)], turns))).toEqual({});
  });

  it('is empty for no turns and for no rows', () => {
    expect(partitionTranscript([mkRow(1, S0)], []).size).toBe(0);
    expect(partitionTranscript([], turns).size).toBe(0);
  });

  it('omits a turn that got no rows instead of mapping it to an empty array', () => {
    const map = partitionTranscript([mkRow(1, S2)], turns);
    expect([...map.keys()]).toEqual(['c']);
    expect(map.has('a')).toBe(false);
    expect(map.get('b')).toBeUndefined();
  });

  it('sorts the turns itself, so an out-of-order slice partitions identically', () => {
    const shuffled = [mkTurnAt('c', S2), mkTurnAt('a', S0), mkTurnAt('b', S1)];
    const rows = [mkRow(1, MID_A), mkRow(2, MID_B), mkRow(3, AFTER_S2)];
    expect(byTurn(partitionTranscript(rows, shuffled))).toEqual({ a: [1], b: [2], c: [3] });
    expect(byTurn(partitionTranscript(rows, shuffled))).toEqual(byTurn(partitionTranscript(rows, turns)));
  });

  it('keys the window off started_at when it is present', () => {
    // `b` was created before `a` even opened but did not start until S2, so the
    // S1 row is `a`'s; keying off created_at would hand it to `b`.
    const late = mkTurnAt('b', S2, BEFORE_S0);
    const pinned = [mkTurnAt('a', S0), late];
    expect(byTurn(partitionTranscript([mkRow(1, S1), mkRow(2, S2)], pinned))).toEqual({ a: [1], b: [2] });
  });

  it('falls back to created_at for a turn that has no start yet', () => {
    const nullStart: TraceTurnSummary = { ...mkTurn('b', 'pending'), created_at: S1, started_at: null };
    const absentStart: TraceTurnSummary = { ...mkTurn('c', 'pending'), created_at: S2, started_at: undefined };
    const pinned = [mkTurnAt('a', S0), nullStart, absentStart];
    const rows = [mkRow(1, MID_A), mkRow(2, MID_B), mkRow(3, AFTER_S2)];
    expect(byTurn(partitionTranscript(rows, pinned))).toEqual({ a: [1], b: [2], c: [3] });
  });

  it('preserves the input order of the rows inside a bucket', () => {
    const rows = [mkRow(3, MID_A), mkRow(1, MID_A), mkRow(2, MID_A)];
    expect(byTurn(partitionTranscript(rows, turns))).toEqual({ a: [3, 1, 2] });
  });

  it('parks a row with an unparseable timestamp in the newest turn', () => {
    // The explicit NaN branch: an unplaceable row still shows up somewhere
    // rather than silently disappearing from the transcript.
    expect(byTurn(partitionTranscript([mkRow(1, 'not-a-timestamp')], turns))).toEqual({ c: [1] });
  });
});
