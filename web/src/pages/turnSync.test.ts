import { describe, it, expect } from 'vitest';

import {
  applyTurnState,
  routeInboundFrame,
  type Frame,
  type SessionView,
  type TranscriptRow,
  type WorkStep,
} from '@aura/chat-core';

// These suites pin the server-authoritative TurnState contract on the
// client: a tab's "Working…" box opens/closes only from the server's
// `turn_state`, never inferred from replayed step history, and a finished
// turn can never be resurrected into a phantom box whose elapsed counts
// from an old start. Regression guard for the multi-tab `/stop` desync.

const SID = 's1';

function workRow(opts: {
  startedAt: number;
  active: boolean;
  endedAt?: number;
  steps?: WorkStep[];
}): TranscriptRow {
  return {
    key: `work-${opts.startedAt}`,
    role: 'system',
    text: '',
    kind: 'work',
    steps: opts.steps ?? [],
    workActive: opts.active,
    workStartedAt: opts.startedAt,
    workEndedAt: opts.endedAt,
  };
}

const toolStep = (label: string): WorkStep => ({
  key: `tool-${label}`,
  kind: 'tool',
  tool: 'edit_file',
  toolLabel: label,
  toolStatus: 'ok',
});

const proseStep = (text: string): WorkStep => ({ key: `prose-${text}`, kind: 'prose', text });

describe('applyTurnState — turn-state reconciliation', () => {
  it('ignores active:true with a null start (stale/lossy artifact — never fabricates a block)', () => {
    // The exact phantom shape: a closed block from a finished turn, plus an
    // `active:true` frame that carries no start (a stale cached turn folded
    // in after a dropped close frame). Must be a no-op.
    const prev = [workRow({ startedAt: 1000, active: false, endedAt: 1500, steps: [toolStep('USER.md')] })];
    expect(applyTurnState(prev, true, null)).toBe(prev);
  });

  it('does not resurrect a *different* finished turn — opens a fresh empty block instead', () => {
    const closed = workRow({ startedAt: 1000, active: false, endedAt: 1500, steps: [toolStep('USER.md')] });
    const next = applyTurnState([closed], true, 2000);
    expect(next).toHaveLength(2);
    expect(next[0]).toBe(closed); // old block untouched: no step resurrection, no re-anchor
    expect(next[1].workActive).toBe(true);
    expect(next[1].workStartedAt).toBe(2000);
    expect(next[1].steps).toEqual([]);
  });

  it('re-opens a closed block whose start matches the in-flight turn (REST reload reconstruction)', () => {
    const steps = [toolStep('x')];
    const closed = workRow({ startedAt: 1000, active: false, endedAt: 1500, steps });
    const next = applyTurnState([closed], true, 1000);
    expect(next).toHaveLength(1);
    expect(next[0].workActive).toBe(true);
    expect(next[0].workEndedAt).toBeUndefined();
    expect(next[0].workStartedAt).toBe(1000);
    expect(next[0].steps).toEqual(steps); // keeps its reconstructed steps
  });

  it('re-pins an already-open block to the server start, staying active', () => {
    const next = applyTurnState([workRow({ startedAt: 999, active: true })], true, 1234);
    expect(next[0].workActive).toBe(true);
    expect(next[0].workStartedAt).toBe(1234);
  });

  it('is idempotent for an open block already at the server start', () => {
    const prev = [workRow({ startedAt: 1234, active: true })];
    expect(applyTurnState(prev, true, 1234)).toBe(prev);
  });

  it('opens a working block for a late joiner with no work block yet', () => {
    const next = applyTurnState([], true, 5000);
    expect(next).toHaveLength(1);
    expect(next[0].kind).toBe('work');
    expect(next[0].workActive).toBe(true);
    expect(next[0].workStartedAt).toBe(5000);
  });

  it('closes an open block on active:false', () => {
    const next = applyTurnState([workRow({ startedAt: 1000, active: true, steps: [toolStep('x')] })], false, null);
    expect(next[0].workActive).toBe(false);
    expect(typeof next[0].workEndedAt).toBe('number');
  });

  it('drops an empty open block on active:false (no `Worked 0s` artifact)', () => {
    expect(applyTurnState([workRow({ startedAt: 1000, active: true, steps: [] })], false, null)).toHaveLength(0);
  });

  it('collapses a prose-tailed open block on active:false (answer streams in its own bubble)', () => {
    const prev = [workRow({ startedAt: 1000, active: true, steps: [proseStep('intermediate')] })];
    const next = applyTurnState(prev, false, null);
    expect(next[0].workActive).toBe(false);
    expect(typeof next[0].workEndedAt).toBe('number');
  });
});

/** A simulated chat tab: drives the real {@link routeInboundFrame} with a
 *  fake `setViews` store so a frame stream mutates an in-memory views map
 *  exactly as it would in the component. */
function makeTab() {
  let views: Record<string, SessionView> = {};
  const setViews = ((u) => {
    views = typeof u === 'function' ? (u as (p: typeof views) => typeof views)(views) : u;
  }) as Parameters<typeof routeInboundFrame>[1];
  const setSessions = (() => {}) as Parameters<typeof routeInboundFrame>[2];
  return {
    views: () => views,
    turn: () => views[SID]?.turn ?? null,
    hasActiveWork: () => (views[SID]?.transcript ?? []).some((r) => r.kind === 'work' && r.workActive),
    feed(frame: Frame) {
      routeInboundFrame(frame, setViews, setSessions, 0);
    },
  };
}

const STARTED = '2026-06-14T12:00:00.000Z';
const STARTED_MS = Date.parse(STARTED);
const turnStart: Frame = { kind: 'turn_state', session_id: SID, active: true, started_at: STARTED };
const turnEnd: Frame = { kind: 'turn_state', session_id: SID, active: false };
const toolStarted: Frame = { kind: 'tool_started', session_id: SID, call_id: 'c1', tool: 'edit_file', label: 'USER.md' };

describe('multi-tab turn sync via routeInboundFrame', () => {
  it('every tab shows the turn active from the same broadcast turn_state', () => {
    const a = makeTab();
    const b = makeTab();
    [a, b].forEach((t) => t.feed(turnStart));
    for (const t of [a, b]) {
      expect(t.turn()).toEqual({ active: true, startedAt: STARTED_MS });
      expect(t.hasActiveWork()).toBe(true);
    }
  });

  it('a /stop (turn_state active:false) clears the working box on the originator AND a pure observer', () => {
    const originator = makeTab();
    const observer = makeTab();
    // Originator streamed a tool step; observer only ever saw turn_state.
    originator.feed(turnStart);
    originator.feed(toolStarted);
    observer.feed(turnStart);
    expect(originator.hasActiveWork()).toBe(true);
    expect(observer.hasActiveWork()).toBe(true);

    [originator, observer].forEach((t) => t.feed(turnEnd));
    for (const t of [originator, observer]) {
      expect(t.turn()).toEqual({ active: false, startedAt: null });
      expect(t.hasActiveWork()).toBe(false);
    }
  });

  it('a repeated idle snapshot on reconnect is idempotent — no phantom box', () => {
    const tab = makeTab();
    tab.feed(turnStart);
    tab.feed(toolStarted);
    tab.feed(turnEnd);
    expect(tab.hasActiveWork()).toBe(false);
    const afterCancel = JSON.stringify(tab.views());
    // Reconnect: the gateway snapshots TurnState{active:false} on Subscribe.
    tab.feed(turnEnd);
    tab.feed(turnEnd);
    expect(tab.hasActiveWork()).toBe(false);
    expect(JSON.stringify(tab.views())).toBe(afterCancel);
  });

  it('a stale active:true with no start cannot resurrect a cancelled turn', () => {
    const tab = makeTab();
    tab.feed(turnStart);
    tab.feed(toolStarted);
    tab.feed(turnEnd);
    expect(tab.hasActiveWork()).toBe(false);
    // The regression trigger: an active frame whose start is absent — the
    // shape a stale cached turn / lossy reconnect snapshot would take.
    tab.feed({ kind: 'turn_state', session_id: SID, active: true });
    expect(tab.hasActiveWork()).toBe(false);
  });

  it('a late tool frame after a /stop cancel does not spawn a phantom Working box', () => {
    // Reproduces the real bug: the cancelled turn had an Edit tool call
    // that *completed after* the cancel. The trailing tool frames arrive
    // while turn.active === false and must fold into the closed block, not
    // open a fresh ticking block anchored to receive-time.
    const tab = makeTab();
    tab.feed(turnStart);
    tab.feed(toolStarted);
    expect(tab.hasActiveWork()).toBe(true);
    tab.feed(turnEnd); // /stop closes the block
    expect(tab.hasActiveWork()).toBe(false);
    // Edit tool finishes after the cancel — late frames, turn already ended:
    tab.feed({ kind: 'tool_started', session_id: SID, call_id: 'c2', tool: 'edit_file', label: 'USER.md' });
    tab.feed({ kind: 'tool_completed', session_id: SID, call_id: 'c2', status: 'ok', summary: 'edited' });
    expect(tab.hasActiveWork()).toBe(false);
  });

  it('a late reasoning frame after the turn ended also stays collapsed', () => {
    const tab = makeTab();
    tab.feed(turnStart);
    tab.feed(turnEnd);
    expect(tab.hasActiveWork()).toBe(false);
    tab.feed({ kind: 'reasoning', session_id: SID, text: 'trailing thought' });
    expect(tab.hasActiveWork()).toBe(false);
  });

  it('an observer that only ever saw turn_state still tracks the full lifecycle', () => {
    const obs = makeTab();
    obs.feed(turnStart);
    expect(obs.hasActiveWork()).toBe(true);
    expect(obs.turn()?.active).toBe(true);
    obs.feed(turnEnd);
    expect(obs.hasActiveWork()).toBe(false);
    expect(obs.turn()?.active).toBe(false);
  });

  it('a terminal notice ends the turn, so a frame landing after it cannot open a new working block', () => {
    // The observed multi-tab bug: the /stop notice arrives, then a late frame
    // (a tool finishing post-cancel, a paced answer flush) lands while
    // `turn.active` is still true — opening a fresh ticking block BELOW the
    // notice. The terminal notice must mark the turn ended so that frame folds
    // into the closed block instead.
    const tab = makeTab();
    tab.feed(turnStart);
    tab.feed(toolStarted);
    expect(tab.hasActiveWork()).toBe(true);

    const stopNotice: Frame = {
      kind: 'notice',
      session_id: SID,
      level: 'info',
      text: 'Stopped. - Cancelled the in-progress reply.',
    };
    tab.feed(stopNotice);
    expect(tab.hasActiveWork()).toBe(false);
    expect(tab.turn()?.active).toBe(false);

    // Late frame after the notice — must NOT resurrect an active block.
    tab.feed({ kind: 'tool_started', session_id: SID, call_id: 'late', tool: 'edit_file', label: 'x' });
    expect(tab.hasActiveWork()).toBe(false);
    tab.feed({ kind: 'reasoning', session_id: SID, text: 'trailing thought' });
    expect(tab.hasActiveWork()).toBe(false);
  });

  it('a /stop cancellation notice labels the block "Cancelled" on an observer tab', () => {
    // The OBSERVER never ran the optimistic /stop (it didn't type it), so the
    // broadcast cancellation notice is the only thing that can label its block.
    const obs = makeTab();
    obs.feed(turnStart);
    obs.feed(toolStarted);
    expect(obs.hasActiveWork()).toBe(true);

    obs.feed({
      kind: 'notice',
      session_id: SID,
      level: 'info',
      text: 'Stopped.\n- Cancelled the in-progress reply.',
    });

    const work = (obs.views()[SID]?.transcript ?? []).find((r) => r.kind === 'work');
    expect(work?.workActive).toBe(false);
    expect(work?.workCancelled).toBe(true);
  });

  it('marks the block cancelled even when turn_state{inactive} closed it first', () => {
    // Frame order races: turn_state{inactive} can collapse the block to
    // "Worked" before the cancellation notice lands. The notice must still
    // re-label the now-closed block.
    const tab = makeTab();
    tab.feed(turnStart);
    tab.feed(toolStarted);
    tab.feed(turnEnd); // closes to "Worked" (no cancel flag on this path)
    expect(tab.hasActiveWork()).toBe(false);

    tab.feed({
      kind: 'notice',
      session_id: SID,
      level: 'info',
      text: 'Stopped.\n- Cancelled the in-progress reply.',
    });
    const work = (tab.views()[SID]?.transcript ?? []).find((r) => r.kind === 'work');
    expect(work?.workCancelled).toBe(true);
  });

  it('a non-cancelling notice leaves the block as plain "Worked"', () => {
    const tab = makeTab();
    tab.feed(turnStart);
    tab.feed(toolStarted);
    tab.feed({ kind: 'notice', session_id: SID, level: 'info', text: 'Context compacted.' });
    const work = (tab.views()[SID]?.transcript ?? []).find((r) => r.kind === 'work');
    expect(work?.workActive).toBe(false);
    expect(work?.workCancelled).toBeFalsy();
  });
});
