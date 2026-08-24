/**
 * Pins the subagent lineage forest: where a child session attaches in its
 * parent's tree, how "something failed / something is still running" rolls up
 * ACROSS the session boundary, and that both roll-ups terminate on a cyclic
 * lineage.
 *
 * Source of truth: `GET /v1/traces/{session_id}/lineage` (the `LineageSession`
 * rows) — the client never re-derives parentage, it only indexes what the
 * server sent, so every rule here is about that indexing and the two
 * recursions over it. The liveness rule deliberately diverges from
 * `isTurnLive` in `traceTreeModel`: `stuck` is live for a turn row but NOT for
 * a subagent, because a stuck child streams nothing and must not pin the page
 * to the fast poll tier forever.
 */
import { describe, expect, it } from 'vitest';
import type {
  LifecycleState,
  LineageSession,
  ReplayStep,
  Span,
  TraceTurnSummary,
  TurnStatusKind,
  TurnTrace,
} from '../../types/trace';
import { isTurnLive } from './traceTreeModel';
import {
  buildForest,
  liveSessions,
  sessionHasFailure,
  sessionIsLive,
} from './traceForest';

const T0 = '2026-01-01T00:00:00.000Z';
const T1 = '2026-01-01T00:00:01.000Z';

const NO_TRACES = new Map<string, TurnTrace>();

function mkTurn(turnId: string, status: TurnStatusKind): TraceTurnSummary {
  return {
    turn_id: turnId,
    session_id: 's',
    turn_status_kind: status,
    turn_input_kind: 'spawned',
    created_at: T0,
    started_at: T0,
    ended_at: status === 'completed' ? T1 : null,
    input_tokens: 0,
    output_tokens: 0,
    cached_input_tokens: 0,
    cache_creation_input_tokens: 0,
  };
}

interface ChildOpts {
  parent?: string;
  /** `undefined` leaves the field absent (pre-field row); `null` / `''` are the
   *  two shapes the wire actually carries for "not recorded". */
  span?: string | null;
  turn?: string;
  turns?: TraceTurnSummary[];
}

function mkChild(sessionId: string, opts: ChildOpts = {}): LineageSession {
  const child: LineageSession = {
    session_id: sessionId,
    parent_session_id: opts.parent ?? 'root',
    parent_turn_id: opts.turn ?? 'turn-root',
    external_agent: null,
    subagent_type: null,
    turns: opts.turns ?? [mkTurn(`${sessionId}-t1`, 'completed')],
  };
  if ('span' in opts) child.parent_span_id = opts.span;
  return child;
}

const ok: LifecycleState = { outcome: 'ok' };
const failed: LifecycleState = { outcome: 'failed', reason: 'boom' };
const cancelled: LifecycleState = { outcome: 'cancelled', reason: 'user_stopped' };

function mkSpan(id: string, outcome: LifecycleState): Span {
  return {
    id,
    step_id: 'step-1',
    kind: {
      kind: 'llm_call',
      begin: { model_id: 'm', provider: 'p', input_messages: [] },
      result: null,
    },
    parallel_group: null,
    started_at: T0,
    ended_at: T1,
    outcome,
  };
}

function mkStep(spans: Span[]): ReplayStep {
  return {
    step: {
      id: 'step-1',
      turn_id: 'turn',
      kind: { kind: 'llm_iteration' },
      started_at: T0,
      ended_at: T1,
      outcome: ok,
    },
    spans,
  };
}

function mkTrace(turnId: string, spans: Span[]): TurnTrace {
  return {
    turn_id: turnId,
    session_id: 's',
    turn_status_kind: 'completed',
    turn_input_kind: 'spawned',
    steps: [mkStep(spans)],
  };
}

function ids(sessions: LineageSession[] | undefined): string[] {
  return (sessions ?? []).map((s) => s.session_id);
}

describe('buildForest', () => {
  it('indexes a child by the spawn_subagent span it hangs under', () => {
    const forest = buildForest([mkChild('c1', { span: 'span-9', turn: 'turn-a' })]);
    expect(ids(forest.bySpan.get('span-9'))).toEqual(['c1']);
    // Indexed at the span means NOT also duplicated at the turn.
    expect(forest.byTurn.get('turn-a')).toBeUndefined();
    expect(forest.byId.get('c1')?.session_id).toBe('c1');
  });

  it('falls back to the parent turn when the spawning span was never recorded', () => {
    // Three shapes of "not recorded": absent field, explicit null, empty string.
    const forest = buildForest([
      mkChild('absent', { turn: 'turn-a' }),
      mkChild('null', { span: null, turn: 'turn-a' }),
      mkChild('empty', { span: '', turn: 'turn-b' }),
    ]);
    expect(ids(forest.byTurn.get('turn-a'))).toEqual(['absent', 'null']);
    expect(ids(forest.byTurn.get('turn-b'))).toEqual(['empty']);
    expect(forest.bySpan.size).toBe(0);
    // An empty-string span id must never become a bucket key of its own — a
    // child parked there would render nowhere.
    expect(forest.bySpan.get('')).toBeUndefined();
  });

  it('keeps two children of the same span together, in input order', () => {
    const forest = buildForest([
      mkChild('second', { span: 'span-9' }),
      mkChild('first', { span: 'span-9' }),
    ]);
    expect(ids(forest.bySpan.get('span-9'))).toEqual(['second', 'first']);
  });

  it('groups grandchildren under their own parent session, not the root', () => {
    const forest = buildForest([
      mkChild('a', { parent: 'root', span: 'span-1' }),
      mkChild('b', { parent: 'root', span: 'span-2' }),
      mkChild('a1', { parent: 'a', span: 'span-3' }),
      mkChild('a2', { parent: 'a', span: 'span-4' }),
    ]);
    expect(ids(forest.byParentSession.get('root'))).toEqual(['a', 'b']);
    expect(ids(forest.byParentSession.get('a'))).toEqual(['a1', 'a2']);
    expect(forest.byParentSession.get('b')).toBeUndefined();
    expect([...forest.byId.keys()]).toEqual(['a', 'b', 'a1', 'a2']);
  });

  it('is empty for an empty lineage', () => {
    const forest = buildForest([]);
    expect(forest.bySpan.size).toBe(0);
    expect(forest.byTurn.size).toBe(0);
    expect(forest.byId.size).toBe(0);
    expect(forest.byParentSession.size).toBe(0);
  });
});

describe('sessionHasFailure', () => {
  it('is true when the child has a failed or stuck turn of its own', () => {
    const failedChild = buildForest([mkChild('c', { turns: [mkTurn('t1', 'failed')] })]);
    const stuckChild = buildForest([mkChild('c', { turns: [mkTurn('t1', 'stuck')] })]);
    expect(sessionHasFailure(failedChild, 'c', NO_TRACES)).toBe(true);
    expect(sessionHasFailure(stuckChild, 'c', NO_TRACES)).toBe(true);
  });

  it('is false for an all-completed subtree, and for an unknown session', () => {
    const forest = buildForest([
      mkChild('c', { turns: [mkTurn('t1', 'completed'), mkTurn('t2', 'completed')] }),
      mkChild('g', { parent: 'c', turns: [mkTurn('t3', 'completed')] }),
    ]);
    expect(sessionHasFailure(forest, 'c', NO_TRACES)).toBe(false);
    expect(sessionHasFailure(forest, 'nope', NO_TRACES)).toBe(false);
  });

  it('crosses the session boundary: a failing GRANDCHILD badges its ancestor', () => {
    const forest = buildForest([
      mkChild('c', { turns: [mkTurn('t1', 'completed')] }),
      mkChild('g', { parent: 'c', turns: [mkTurn('t2', 'completed')] }),
      mkChild('gg', { parent: 'g', turns: [mkTurn('t3', 'failed')] }),
    ]);
    expect(sessionHasFailure(forest, 'gg', NO_TRACES)).toBe(true);
    expect(sessionHasFailure(forest, 'g', NO_TRACES)).toBe(true);
    expect(sessionHasFailure(forest, 'c', NO_TRACES)).toBe(true);
  });

  it('counts a failing span in a LOADED trace even when the turn says completed', () => {
    // A turn can report `completed` while a span inside it failed and was
    // recovered — the status roll-up alone would call that subtree clean.
    const forest = buildForest([mkChild('c', { turns: [mkTurn('t1', 'completed')] })]);
    const traces = new Map<string, TurnTrace>([['t1', mkTrace('t1', [mkSpan('sp', failed)])]]);
    expect(sessionHasFailure(forest, 'c', traces)).toBe(true);
    // A loaded-but-clean trace does not invent one.
    const clean = new Map<string, TurnTrace>([['t1', mkTrace('t1', [mkSpan('sp', ok)])]]);
    expect(sessionHasFailure(forest, 'c', clean)).toBe(false);
    // A trace loaded for some OTHER turn is not consulted.
    const elsewhere = new Map<string, TurnTrace>([['other', mkTrace('other', [mkSpan('sp', failed)])]]);
    expect(sessionHasFailure(forest, 'c', elsewhere)).toBe(false);
  });

  it('picks up a loaded grandchild trace through the parent', () => {
    const forest = buildForest([
      mkChild('c', { turns: [mkTurn('t1', 'completed')] }),
      mkChild('g', { parent: 'c', turns: [mkTurn('t2', 'completed')] }),
    ]);
    const traces = new Map<string, TurnTrace>([['t2', mkTrace('t2', [mkSpan('sp', failed)])]]);
    expect(sessionHasFailure(forest, 'c', traces)).toBe(true);
  });

  it('treats a cancelled turn and a cancelled span differently (status vs loaded trace)', () => {
    // Documented divergence, not an assertion that it is right: `turnFailed`
    // ignores `cancelled` while `failureCount` counts a cancelled span, so the
    // same user-stopped subagent goes from unbadged to badged the moment its
    // step tree loads.
    const forest = buildForest([mkChild('c', { turns: [mkTurn('t1', 'cancelled')] })]);
    expect(sessionHasFailure(forest, 'c', NO_TRACES)).toBe(false);
    const traces = new Map<string, TurnTrace>([['t1', mkTrace('t1', [mkSpan('sp', cancelled)])]]);
    expect(sessionHasFailure(forest, 'c', traces)).toBe(true);
  });

  it('does not leak its visited set between top-level calls', () => {
    // The `seen` guard is a default parameter; if it were hoisted to a shared
    // instance the second render of the same row would report clean.
    const forest = buildForest([mkChild('c', { turns: [mkTurn('t1', 'failed')] })]);
    expect(sessionHasFailure(forest, 'c', NO_TRACES)).toBe(true);
    expect(sessionHasFailure(forest, 'c', NO_TRACES)).toBe(true);
    expect(sessionIsLive(forest, 'c')).toBe(false);
  });
});

describe('cycle safety', () => {
  // A lineage where A is B's parent AND B is A's parent. The server should
  // never emit this, but the roll-ups are the only recursion in the viewer and
  // a cycle here is an unbounded recursion in a render path — every assertion
  // below fails with a RangeError instead of a wrong value if the guard goes.
  const cyclic = (aTurns: TraceTurnSummary[], bTurns: TraceTurnSummary[]): LineageSession[] => [
    { ...mkChild('a', { parent: 'b', span: 'span-b' }), turns: aTurns },
    { ...mkChild('b', { parent: 'a', span: 'span-a' }), turns: bTurns },
  ];

  it('terminates on a 2-cycle with no failure and no live turn', () => {
    const forest = buildForest(cyclic([mkTurn('t1', 'completed')], [mkTurn('t2', 'completed')]));
    expect(sessionHasFailure(forest, 'a', NO_TRACES)).toBe(false);
    expect(sessionHasFailure(forest, 'b', NO_TRACES)).toBe(false);
    expect(sessionIsLive(forest, 'a')).toBe(false);
    expect(sessionIsLive(forest, 'b')).toBe(false);
  });

  it('still finds the failure / the live turn around the cycle', () => {
    const failing = buildForest(cyclic([mkTurn('t1', 'completed')], [mkTurn('t2', 'failed')]));
    expect(sessionHasFailure(failing, 'a', NO_TRACES)).toBe(true);
    expect(sessionHasFailure(failing, 'b', NO_TRACES)).toBe(true);

    const live = buildForest(cyclic([mkTurn('t1', 'completed')], [mkTurn('t2', 'in_progress')]));
    expect(sessionIsLive(live, 'a')).toBe(true);
    expect(sessionIsLive(live, 'b')).toBe(true);
    expect(liveSessions(live)).toEqual(['b']);
  });

  it('terminates on a self-parenting session and on a 3-cycle', () => {
    const selfLoop = buildForest([
      { ...mkChild('a', { parent: 'a' }), turns: [mkTurn('t1', 'completed')] },
    ]);
    expect(sessionHasFailure(selfLoop, 'a', NO_TRACES)).toBe(false);
    expect(sessionIsLive(selfLoop, 'a')).toBe(false);

    const ring = buildForest([
      { ...mkChild('a', { parent: 'c' }), turns: [mkTurn('t1', 'completed')] },
      { ...mkChild('b', { parent: 'a' }), turns: [mkTurn('t2', 'completed')] },
      { ...mkChild('c', { parent: 'b' }), turns: [mkTurn('t3', 'stuck')] },
    ]);
    expect(sessionHasFailure(ring, 'a', NO_TRACES)).toBe(true);
    expect(sessionIsLive(ring, 'a')).toBe(false);
  });
});

describe('sessionIsLive', () => {
  it('is true for a pending or in_progress turn', () => {
    const pending = buildForest([mkChild('c', { turns: [mkTurn('t1', 'pending')] })]);
    const running = buildForest([mkChild('c', { turns: [mkTurn('t1', 'in_progress')] })]);
    expect(sessionIsLive(pending, 'c')).toBe(true);
    expect(sessionIsLive(running, 'c')).toBe(true);
  });

  it('is FALSE for stuck — narrower than isTurnLive on purpose', () => {
    // A stuck child streams nothing new; counting it live would hold the page
    // on the fast poll tier forever. The turn-row rule says the opposite.
    const forest = buildForest([mkChild('c', { turns: [mkTurn('t1', 'stuck')] })]);
    expect(sessionIsLive(forest, 'c')).toBe(false);
    expect(isTurnLive('stuck')).toBe(true);
  });

  it('is false for every terminal status and for an unknown session', () => {
    for (const status of ['completed', 'failed', 'cancelled'] as const) {
      const forest = buildForest([mkChild('c', { turns: [mkTurn('t1', status)] })]);
      expect(sessionIsLive(forest, 'c')).toBe(false);
    }
    expect(sessionIsLive(buildForest([]), 'c')).toBe(false);
  });

  it('crosses the session boundary: a running grandchild keeps its ancestor live', () => {
    const forest = buildForest([
      mkChild('c', { turns: [mkTurn('t1', 'completed')] }),
      mkChild('g', { parent: 'c', turns: [mkTurn('t2', 'completed')] }),
      mkChild('gg', { parent: 'g', turns: [mkTurn('t3', 'in_progress')] }),
    ]);
    expect(sessionIsLive(forest, 'c')).toBe(true);
    expect(sessionIsLive(forest, 'g')).toBe(true);
    // A sibling branch with nothing running stays idle.
    const idle = buildForest([mkChild('x', { turns: [mkTurn('t4', 'completed')] })]);
    expect(sessionIsLive(idle, 'x')).toBe(false);
  });

  it('is true when any one turn of a session runs', () => {
    const forest = buildForest([
      mkChild('c', { turns: [mkTurn('t1', 'completed'), mkTurn('t2', 'in_progress')] }),
    ]);
    expect(sessionIsLive(forest, 'c')).toBe(true);
  });
});

describe('liveSessions', () => {
  it('returns exactly the sessions with a running turn', () => {
    const forest = buildForest([
      mkChild('idle', { turns: [mkTurn('t1', 'completed')] }),
      mkChild('running', { turns: [mkTurn('t2', 'in_progress')] }),
      mkChild('queued', { parent: 'running', turns: [mkTurn('t3', 'pending')] }),
      mkChild('stuck', { turns: [mkTurn('t4', 'stuck')] }),
      mkChild('broken', { turns: [mkTurn('t5', 'failed')] }),
    ]);
    expect(liveSessions(forest)).toEqual(['running', 'queued']);
  });

  it('is [] when nothing runs, and [] for an empty forest', () => {
    const forest = buildForest([
      mkChild('a', { turns: [mkTurn('t1', 'completed')] }),
      mkChild('b', { turns: [mkTurn('t2', 'cancelled')] }),
    ]);
    expect(liveSessions(forest)).toEqual([]);
    expect(liveSessions(buildForest([]))).toEqual([]);
    expect(liveSessions(buildForest([]))).toEqual([]);
  });

  it('reports a session with no turns at all as not live', () => {
    const forest = buildForest([mkChild('c', { turns: [] })]);
    expect(liveSessions(forest)).toEqual([]);
    expect(sessionIsLive(forest, 'c')).toBe(false);
    expect(sessionHasFailure(forest, 'c', NO_TRACES)).toBe(false);
  });
});
