import { describe, it, expect } from 'vitest';

import {
  applySyncMerge,
  applyTurnState,
  routeInboundFrame,
  transcriptItemToRow,
  type SessionView,
  type TranscriptRow,
  type WorkStep,
} from './ChatPage';
import type { components } from '../api/schema';
import type { Frame } from '../api/chatWs';

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

  it('finds the matching block across its trailing answer/notice run (no duplicate below)', () => {
    // The turn's partial answer bubble and a committed notice row land BELOW
    // the block, so the block is not the literal tail; the reconciliation must
    // still reach it rather than opening a second card after them.
    const closed = workRow({ startedAt: 1000, active: false, endedAt: 1500, steps: [toolStep('x')] });
    const answer: TranscriptRow = { key: 'a', role: 'assistant', text: 'partial' };
    const notice: TranscriptRow = {
      key: 'n',
      role: 'system',
      text: '',
      notice: { level: 'warn', text: 'degraded' },
    };
    const next = applyTurnState([closed, answer, notice], true, 1000);
    expect(next).toHaveLength(3);
    expect(next[0].workActive).toBe(true);
    expect(next[0].workEndedAt).toBeUndefined();
    expect(next[1]).toBe(answer);
    expect(next[2]).toBe(notice);
  });

  it('a user row is the barrier — an earlier turn is never reconciled through it', () => {
    const closed = workRow({ startedAt: 1000, active: false, endedAt: 1500, steps: [toolStep('x')] });
    const user: TranscriptRow = { key: 'u', role: 'user', text: 'next prompt' };
    const next = applyTurnState([closed, user], true, 2000);
    expect(next).toHaveLength(3);
    expect(next[0]).toBe(closed); // untouched
    expect(next[2].kind).toBe('work');
    expect(next[2].workActive).toBe(true);
    expect(next[2].workStartedAt).toBe(2000);
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
      routeInboundFrame(frame, setViews, setSessions);
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

  it('a mid_turn notice folds into the ACTIVE block; the turn keeps running', () => {
    // Committing the notice as its own row would sever the block — no longer
    // the transcript tail — so the turn's next work frame would fork a second
    // "Worked" card. It folds in as a leveled step instead (the iOS model).
    // Fold-eligibility is the SERVER's `mid_turn` declaration (tool asides),
    // never inferred from timing.
    const tab = makeTab();
    tab.feed(turnStart);
    tab.feed(toolStarted);
    tab.feed({ kind: 'notice', session_id: SID, level: 'warn', text: 'degraded mode', mid_turn: true });
    const work = tab.views()[SID].transcript.find((r) => r.kind === 'work');
    expect(work?.workActive).toBe(true); // not severed — the turn is still going
    const foldedSteps = work?.steps ?? [];
    expect(foldedSteps[foldedSteps.length - 1]).toMatchObject({
      kind: 'notice',
      noticeLevel: 'warn',
      text: 'degraded mode',
    });
    expect(tab.views()[SID].transcript.some((r) => r.notice !== undefined)).toBe(false);

    // More work after the notice lands in the SAME card…
    tab.feed({ kind: 'tool_started', session_id: SID, call_id: 'c2', tool: 'edit_file', label: 'y' });
    expect(tab.views()[SID].transcript.filter((r) => r.kind === 'work')).toHaveLength(1);

    // …and the turn's own end closes it to a plain "Worked" (never "Cancelled").
    tab.feed(turnEnd);
    const closed = tab.views()[SID].transcript.find((r) => r.kind === 'work');
    expect(closed?.workActive).toBe(false);
    expect(closed?.workCancelled).toBeFalsy();
  });

  it('a notice with no active block keeps its committed row (between turns)', () => {
    const tab = makeTab();
    tab.feed({ kind: 'notice', session_id: SID, level: 'info', text: 'Context compacted.' });
    const rows = tab.views()[SID].transcript;
    expect(rows).toHaveLength(1);
    expect(rows[0].notice?.text).toBe('Context compacted.');
    expect(tab.turn()?.active).toBe(false);
  });

  it('a turn-failure notice on the EMPTY eagerly-opened block stays a visible card', () => {
    // The actor emits the "turn failed before producing a reply" error notice
    // BEFORE the projector's turn_state{inactive}, so it lands while the
    // step-less working affordance is still active. Folding it there would
    // bury the turn's only output inside a bare "Worked Xs ›" stub.
    const tab = makeTab();
    tab.feed(turnStart); // opens the empty active block
    tab.feed({
      kind: 'notice',
      session_id: SID,
      level: 'error',
      text: 'The turn failed before producing a reply: boom',
    });
    const rows = tab.views()[SID].transcript;
    expect(rows.some((r) => r.kind === 'work')).toBe(false); // empty block dropped
    expect(rows[rows.length - 1].notice?.level).toBe('error'); // failure visible
    tab.feed(turnEnd);
    expect(tab.views()[SID].transcript.some((r) => r.kind === 'work')).toBe(false);
  });

  it('a mid-stream mid_turn notice folds into the block (bubble → prose, no sever, no fork)', () => {
    const tab = makeTab();
    tab.feed(turnStart);
    tab.feed(toolStarted);
    tab.feed({ kind: 'answer_delta', session_id: SID, text: 'partial ' });
    tab.feed({ kind: 'notice', session_id: SID, level: 'warn', text: 'degraded mode', mid_turn: true });
    const rows = tab.views()[SID].transcript;
    expect(rows.filter((r) => r.kind === 'work')).toHaveLength(1);
    expect(rows.some((r) => r.notice !== undefined)).toBe(false); // folded, not committed
    const work = rows.find((r) => r.kind === 'work');
    expect(work?.workActive).toBe(true); // block not frozen mid-stream
    // The streamed text is preserved as a prose step AHEAD of the notice, and
    // the bubble row is gone — left at the tail it would become the recreated
    // pacer's write target and the next delta would erase the pre-notice text.
    const steps = work?.steps ?? [];
    expect(steps[steps.length - 2]).toMatchObject({ kind: 'prose', text: 'partial ' });
    expect(steps[steps.length - 1]).toMatchObject({ kind: 'notice', text: 'degraded mode' });
    expect(rows[rows.length - 1].kind).toBe('work'); // bubble folded away

    // A post-notice delta opens a FRESH bubble with only its own text…
    tab.feed({ kind: 'answer_delta', session_id: SID, text: 'part B' });
    const rows2 = tab.views()[SID].transcript;
    expect(rows2[rows2.length - 1]).toMatchObject({ role: 'assistant', streaming: true, text: 'part B' });

    // …and the turn's continuation still lands in the SAME card.
    tab.feed({ kind: 'tool_started', session_id: SID, call_id: 'c2', tool: 'edit_file', label: 'y' });
    expect(tab.views()[SID].transcript.filter((r) => r.kind === 'work')).toHaveLength(1);
  });

  it('a no-op /stop ack severs even against a still-active block (persisted ack stays visible)', () => {
    // An observer tab can receive "Nothing in progress to stop." while its
    // block is still active (the router's ack is unordered w.r.t. the turn-end
    // frames). The ack is a persisted control event and carries no `mid_turn`,
    // so it severs — burying it inside the collapsed card would hide it live
    // and double it against the durable row.
    const tab = makeTab();
    tab.feed(turnStart);
    tab.feed(toolStarted);
    tab.feed({ kind: 'notice', session_id: SID, level: 'info', text: 'Nothing in progress to stop.' });
    const rows = tab.views()[SID].transcript;
    expect(rows[rows.length - 1].notice?.text).toBe('Nothing in progress to stop.');
    const work = rows.find((r) => r.kind === 'work');
    expect(work?.workActive).toBe(false); // severed + closed, never "Cancelled"
    expect(work?.workCancelled).toBeFalsy();
  });

  it('a durably-persisted notice keys its live row by the n<seq> id, so the synced twin dedups', () => {
    const tab = makeTab();
    tab.feed({
      kind: 'notice',
      session_id: SID,
      level: 'info',
      text: 'Context compacted.',
      durable_id: 'n7',
    });
    const rows = tab.views()[SID].transcript;
    expect(rows).toHaveLength(1);
    expect(rows[0].key).toBe(`row-${SID}-n7`); // transcriptRowKey(sid, 'n7')
    // The next difference sync redelivers the same control event — the stable
    // key makes applySyncMerge's dedup skip it instead of doubling the card.
    const synced = transcriptItemToRow(SID, {
      id: 'n7',
      kind: 'notice',
      role: '',
      text: 'Context compacted.',
      has_attachments: false,
      notice_level: 'info',
      created_at: '2026-06-14T12:00:05Z',
    } as components['schemas']['ChatTranscriptItem']);
    expect(applySyncMerge(rows, [synced])).toBe(rows); // unchanged — deduped
  });

  it('skips the durable-keyed mint when the row is already on screen (sync raced ahead)', () => {
    // Persist precedes emit, and a gap/reconnect sync is unordered w.r.t. the
    // WS frame — the synced twin can land first. The live frame must not
    // append a second card under the same key.
    const tab = makeTab();
    const noticeFrame: Frame = {
      kind: 'notice',
      session_id: SID,
      level: 'info',
      text: 'Context compacted.',
      durable_id: 'n7',
    };
    tab.feed(noticeFrame);
    tab.feed(noticeFrame);
    expect(tab.views()[SID].transcript).toHaveLength(1);
  });

  it('a terminal failure notice on a STEPPED active block still severs into a visible card', () => {
    // The turn-failed / blank-reply notices carry no `mid_turn`, and they beat
    // the projector's turn_state{inactive}. With work already on the block the
    // old shape-based fold buried them inside the collapsing card; the wire
    // flag keeps them a committed row.
    const tab = makeTab();
    tab.feed(turnStart);
    tab.feed(toolStarted);
    tab.feed({
      kind: 'notice',
      session_id: SID,
      level: 'error',
      text: 'The turn failed before producing a reply: boom',
    });
    const rows = tab.views()[SID].transcript;
    expect(rows[rows.length - 1].notice?.level).toBe('error'); // failure visible
    const work = rows.find((r) => r.kind === 'work');
    expect(work?.workActive).toBe(false); // block collapsed above the card
    expect(work?.steps?.some((s) => s.kind === 'notice')).toBeFalsy(); // not folded
    tab.feed(turnEnd);
    expect(tab.views()[SID].transcript[rows.length - 1].notice?.level).toBe('error');
  });

  it('keeps attachments when an ordinal final message replaces a streaming bubble', () => {
    const tab = makeTab();
    tab.feed({ kind: 'answer_delta', session_id: SID, text: 'Here is the report.' });
    tab.feed({
      kind: 'message',
      session_id: SID,
      user_id: 'web-operator',
      channel_type: 'http',
      role: 'assistant',
      content: 'Here is the report.',
      ordinal: 7,
      attachments: [
        {
          kind: 'file',
          blob_id: 'sha256:report-token',
          mime_type: 'application/pdf',
          size: 42,
          filename: 'report.pdf',
        },
      ],
    });

    const transcript = tab.views()[SID]?.transcript ?? [];
    expect(transcript).toHaveLength(1);
    expect(transcript[0]).toMatchObject({
      key: `row-${SID}-m7`,
      streaming: false,
      hasAttachments: true,
      attachments: [
        {
          kind: 'file',
          blob_id: 'sha256:report-token',
          filename: 'report.pdf',
        },
      ],
    });
  });
});
