import { describe, it, expect } from 'vitest';

import {
  workBlockDisplay,
  formatDuration,
  formatWorkedLabel,
  foldNoticeIntoActiveWork,
  closeActiveWork,
  settleActiveWork,
  finalizeTrailingAnswer,
  isStopCommand,
  isStopCancellationNotice,
  markLastWorkCancelled,
  pushToolStartedStep,
  applyToolCompletedStep,
  markStepAwaitingApproval,
  resolveStepApproval,
  foldAdjacentWork,
  type TranscriptRow,
  type WorkStep,
} from './ChatPage';

// Pins the "Working" affordance UX: a live turn shows a compact spinner and
// only expands into the steps panel once it has actually produced a step,
// and a finished turn never renders a "Worked 0s" artifact.

describe('workBlockDisplay — spinner first, expand on the first step', () => {
  it('live turn with no steps yet is a compact spinner: boxed but panel shut', () => {
    expect(workBlockDisplay(true, false, false)).toEqual({ boxed: true, panelOpen: false });
  });

  it('live turn expands the panel the moment a step lands', () => {
    expect(workBlockDisplay(true, true, false)).toEqual({ boxed: true, panelOpen: true });
  });

  it('finished, collapsed block is neither boxed nor open (the dim summary line)', () => {
    expect(workBlockDisplay(false, true, false)).toEqual({ boxed: false, panelOpen: false });
  });

  it('finished block the user re-expanded is boxed with its panel open', () => {
    expect(workBlockDisplay(false, true, true)).toEqual({ boxed: true, panelOpen: true });
  });

  it('a live turn is never collapsed shut even mid-expand-toggle', () => {
    // `expanded` is meaningless while active; the live turn stays boxed
    // regardless, so toggling can't hide an in-flight turn.
    expect(workBlockDisplay(true, false, true).boxed).toBe(true);
  });

  it('a settling block (interjection-paused) stays boxed with its panel open', () => {
    // workActive=false (reads "Worked"), not user-expanded, but settling keeps
    // it open until the turn ends.
    expect(workBlockDisplay(false, true, false, true)).toEqual({ boxed: true, panelOpen: true });
  });
});

describe('formatDuration — humanized by magnitude, mirrors the iOS transcript', () => {
  it.each([
    ['seconds under a minute', 5_000, '5s'],
    ['the last second before the minute boundary', 59_000, '59s'],
    ['exactly one minute crosses into the m form', 60_000, '1m'],
    ['minutes and seconds', 90_000, '1m 30s'],
    ['a whole minute drops the seconds', 120_000, '2m'],
    ['hours and minutes', 5_400_000, '1h 30m'],
    ['a whole hour drops the minutes', 3_600_000, '1h'],
    ['a 60-minute rounding carry rolls the hour', 7_170_000, '2h'],
  ])('%s', (_why, ms, expected) => {
    expect(formatDuration(ms)).toBe(expected);
  });
});

describe('formatWorkedLabel — never "0s", "Cancelled" for /stop', () => {
  it('renders just "Worked" for a sub-second turn', () => {
    expect(formatWorkedLabel(0)).toBe('Worked');
    expect(formatWorkedLabel(500)).toBe('Worked');
  });

  it('renders the humanized duration once it reaches 1s', () => {
    expect(formatWorkedLabel(1_000)).toBe('Worked 1s');
    expect(formatWorkedLabel(42_000)).toBe('Worked 42s');
    expect(formatWorkedLabel(90_000)).toBe('Worked 1m 30s');
  });

  it('labels a cancelled turn distinctly, still never "0s"', () => {
    expect(formatWorkedLabel(0, true)).toBe('Cancelled');
    expect(formatWorkedLabel(7_000, true)).toBe('Cancelled · Worked 7s');
  });
});

describe('foldNoticeIntoActiveWork — mid-turn notices stay inside the card', () => {
  const step: WorkStep = { key: 's', kind: 'tool', tool: 'edit_file', toolStatus: 'ok' };
  const activeBlock = (): TranscriptRow => ({
    key: 'w',
    role: 'system',
    text: '',
    kind: 'work',
    steps: [step],
    workActive: true,
    workStartedAt: 1000,
  });

  it('folds a leveled notice step into the active tail block, leaving the turn running', () => {
    const out = foldNoticeIntoActiveWork([activeBlock()], 'warn', 'degraded mode');
    expect(out).toHaveLength(1);
    expect(out?.[0].workActive).toBe(true);
    expect(out?.[0].steps).toHaveLength(2);
    expect(out?.[0].steps?.[1]).toMatchObject({
      kind: 'notice',
      noticeLevel: 'warn',
      text: 'degraded mode',
    });
  });

  it('declines a frozen tail — the notice keeps its committed row', () => {
    const frozen: TranscriptRow = { ...activeBlock(), workActive: false, workEndedAt: 1500 };
    expect(foldNoticeIntoActiveWork([frozen], 'error', 'x')).toBeNull();
  });

  it('folds a trailing streaming bubble to prose AHEAD of the notice step', () => {
    // The web keeps the streamed reply as a transcript row below the block
    // (iOS holds it out-of-band). The bubble must fold away, not be reached
    // over: the notice's pacer flush deletes the pacer, and the next delta's
    // recreated pacer would adopt a left-behind bubble as its write target and
    // wholesale-replace — erase — the pre-notice text.
    const bubble: TranscriptRow = { key: 'a', role: 'assistant', text: 'partial', streaming: true };
    const out = foldNoticeIntoActiveWork([activeBlock(), bubble], 'warn', 'degraded');
    expect(out).toHaveLength(1); // bubble folded away
    expect(out?.[0].workActive).toBe(true);
    expect(out?.[0].steps).toMatchObject([
      { kind: 'tool' },
      { kind: 'prose', text: 'partial' },
      { kind: 'notice', noticeLevel: 'warn', text: 'degraded' },
    ]);
  });

  it('drops a blank streaming bubble without minting an empty prose step', () => {
    const blank: TranscriptRow = { key: 'a', role: 'assistant', text: '   ', streaming: true };
    const out = foldNoticeIntoActiveWork([activeBlock(), blank], 'info', 'x');
    expect(out).toHaveLength(1);
    expect(out?.[0].steps).toMatchObject([{ kind: 'tool' }, { kind: 'notice', text: 'x' }]);
  });

  it('declines when the streaming bubble sits over a frozen block', () => {
    const frozen: TranscriptRow = { ...activeBlock(), workActive: false, workEndedAt: 1500 };
    const bubble: TranscriptRow = { key: 'a', role: 'assistant', text: 'partial', streaming: true };
    expect(foldNoticeIntoActiveWork([frozen, bubble], 'info', 'x')).toBeNull();
  });

  it('declines a FINALIZED trailing answer (final-reply phase done — not mid-stream)', () => {
    const done: TranscriptRow = { key: 'a', role: 'assistant', text: 'answer' };
    expect(foldNoticeIntoActiveWork([activeBlock(), done], 'info', 'x')).toBeNull();
  });

  it('declines an EMPTY active block — the eagerly-opened working affordance', () => {
    // `applyTurnState` opens a step-less block on `turn_state{active}` before
    // any work lands (iOS opens none until a real work frame). A notice on it
    // is the turn's ONLY output — the turn-failed / blank-reply notice racing
    // ahead of `turn_state{inactive}` — and must stay a visible card instead
    // of being buried inside a bare "Worked Xs ›" stub.
    const empty: TranscriptRow = { ...activeBlock(), steps: [] };
    expect(foldNoticeIntoActiveWork([empty], 'error', 'The turn failed')).toBeNull();
  });

  it('declines an empty transcript', () => {
    expect(foldNoticeIntoActiveWork([], 'info', 'x')).toBeNull();
  });
});


describe('straggler frames fold back across trailing notice rows (one turn, one card)', () => {
  const closedWork = (): TranscriptRow => ({
    key: 'w',
    role: 'system',
    text: '',
    kind: 'work',
    steps: [{ key: 's', kind: 'tool', tool: 'edit_file', toolStatus: 'ok' }],
    workActive: false,
    workStartedAt: 1000,
    workEndedAt: 1500,
  });
  const noticeRow = (): TranscriptRow => ({
    key: 'n',
    role: 'system',
    text: '',
    notice: { level: 'warn', text: 'stopped' },
  });

  it('a tool completing after the freeze folds into the block, not a second card', () => {
    const out = applyToolCompletedStep([closedWork(), noticeRow()], 'c9', 'ok', 'late result', undefined, false);
    expect(out).toHaveLength(2); // no third row forked
    expect(out[0].kind).toBe('work');
    expect(out[0].steps).toHaveLength(2);
    expect(out[0].steps?.[1]).toMatchObject({ kind: 'tool', toolSummary: 'late result' });
    expect(out[0].workActive).toBe(false); // stays frozen — never re-opened
    expect(out[0].workEndedAt).toBe(1500); // never re-timed
    expect(out[1].notice?.text).toBe('stopped'); // the notice keeps its row
  });

  it('a late tool START also folds back (still one card)', () => {
    const out = pushToolStartedStep([closedWork(), noticeRow()], 'c9', 'Bash', 'ls', false);
    expect(out).toHaveLength(2);
    expect(out[0].steps).toHaveLength(2);
    expect(out[0].workActive).toBe(false);
  });

  it('the scan stops at a non-notice row — a later turn still gets its own card', () => {
    const rows: TranscriptRow[] = [closedWork(), { key: 'a', role: 'assistant', text: 'done' }];
    const out = applyToolCompletedStep(rows, 'c9', 'ok', 'late', undefined, false);
    expect(out).toHaveLength(3);
    expect(out[0].steps).toHaveLength(1); // the finished turn is untouched
    expect(out[2].kind).toBe('work'); // fresh block appended at the tail
  });
});

describe('isStopCommand — optimistic /stop recognition', () => {
  it.each([
    ['/stop', true],
    ['/STOP', true],
    ['  /stop  ', true],
    ['/stop@mybot', true],
    ['/stop now', true],
    ['/stopwatch', false],
    ['/compact', false],
    ['stop', false],
    ['hello /stop', false],
  ])('%s → %s', (input, expected) => {
    expect(isStopCommand(input)).toBe(expected);
  });
});

describe('closeActiveWork — optimistic cancel on /stop', () => {
  const step: WorkStep = { key: 's', kind: 'tool', tool: 'edit_file', toolStatus: 'ok' };
  const activeBlock = (): TranscriptRow => ({
    key: 'w',
    role: 'system',
    text: '',
    kind: 'work',
    steps: [step],
    workActive: true,
    workStartedAt: 1000,
  });

  it('marks the closed block cancelled when stopping', () => {
    const [row] = closeActiveWork([activeBlock()], true);
    expect(row.workActive).toBe(false);
    expect(row.workCancelled).toBe(true);
    expect(typeof row.workEndedAt).toBe('number');
  });

  it('leaves the block uncancelled on a normal close', () => {
    const [row] = closeActiveWork([activeBlock()], false);
    expect(row.workActive).toBe(false);
    expect(row.workCancelled).toBeFalsy();
  });

  it('drops an empty block even when stopping (nothing to label)', () => {
    const empty: TranscriptRow = { ...activeBlock(), steps: [] };
    expect(closeActiveWork([empty], true)).toHaveLength(0);
  });
});

describe('settleActiveWork — interjection pauses the block open until turn-end', () => {
  const step: WorkStep = { key: 's', kind: 'tool', tool: 'edit_file', toolStatus: 'ok' };
  const activeBlock = (): TranscriptRow => ({
    key: 'w',
    role: 'system',
    text: '',
    kind: 'work',
    steps: [step],
    workActive: true,
    workStartedAt: 1000,
  });

  it('relabels to "Worked" (workActive false) but marks the block settling (kept open)', () => {
    const [row] = settleActiveWork([activeBlock()]);
    expect(row.workActive).toBe(false);
    expect(row.workSettling).toBe(true);
    expect(row.workCancelled).toBeFalsy();
    expect(typeof row.workEndedAt).toBe('number');
  });

  it('drops an empty block (no work to keep open)', () => {
    const empty: TranscriptRow = { ...activeBlock(), steps: [] };
    expect(settleActiveWork([empty])).toHaveLength(0);
  });

  it('closeActiveWork collapses a settling block at turn-end (clears the flag)', () => {
    const settled = settleActiveWork([activeBlock()]);
    expect(settled[0].workSettling).toBe(true);
    // A later turn-end close (no active block left) still clears settling so the
    // block collapses to its summary.
    const [row] = closeActiveWork(settled);
    expect(row.workSettling).toBe(false);
    expect(row.workActive).toBe(false);
  });
});

describe('isStopCancellationNotice — only a real cancel', () => {
  it('true when the /stop actually cancelled the reply', () => {
    expect(isStopCancellationNotice('Stopped.\n- Cancelled the in-progress reply.')).toBe(true);
  });
  it('false for a no-op /stop', () => {
    expect(isStopCancellationNotice('Nothing in progress to stop.')).toBe(false);
  });
  it('false for an unrelated notice (e.g. /compact)', () => {
    expect(isStopCancellationNotice('Context compacted.')).toBe(false);
  });
});

describe('markLastWorkCancelled — label the turn just stopped', () => {
  const closedWork = (): TranscriptRow => ({
    key: 'w',
    role: 'system',
    text: '',
    kind: 'work',
    steps: [{ key: 's', kind: 'tool', tool: 'edit_file', toolStatus: 'ok' }],
    workActive: false,
    workStartedAt: 1000,
    workEndedAt: 1500,
  });

  it('marks a trailing closed work block cancelled', () => {
    const [row] = markLastWorkCancelled([closedWork()]);
    expect(row.workCancelled).toBe(true);
  });

  it('never reaches back past a newer non-work tail (dropped-empty case)', () => {
    const rows: TranscriptRow[] = [
      closedWork(),
      { key: 'u', role: 'user', text: 'next prompt' },
    ];
    const out = markLastWorkCancelled(rows);
    expect(out[0].workCancelled).toBeFalsy();
    expect(out).toBe(rows); // unchanged reference — no work tail to mark
  });

  it('marks the work block above a salvaged trailing reply bubble', () => {
    // After a /stop the partial reply is kept as its own bubble below the
    // work block, so the block is no longer the last row — it must still get
    // the "Cancelled" label.
    const rows: TranscriptRow[] = [
      closedWork(),
      { key: 'a', role: 'assistant', text: 'a partial answer' },
    ];
    const out = markLastWorkCancelled(rows);
    expect(out[0].workCancelled).toBe(true);
    expect(out[1].text).toBe('a partial answer'); // bubble untouched
  });
});

describe('finalizeTrailingAnswer — keep the /stop reply as a bubble', () => {
  const streamingAnswer = (text: string): TranscriptRow => ({
    key: 'a',
    role: 'assistant',
    text,
    streaming: true,
  });
  const activeWork = (): TranscriptRow => ({
    key: 'w',
    role: 'system',
    text: '',
    kind: 'work',
    steps: [{ key: 's', kind: 'tool', tool: 'edit_file', toolStatus: 'ok' }],
    workActive: true,
    workStartedAt: 1000,
  });

  it('finalizes a trailing streaming answer into a permanent bubble', () => {
    const out = finalizeTrailingAnswer([activeWork(), streamingAnswer('a b-tree is')]);
    expect(out).toHaveLength(2);
    expect(out[1].role).toBe('assistant');
    expect(out[1].streaming).toBe(false);
    expect(out[1].text).toBe('a b-tree is');
  });

  it('drops an empty partial (nothing streamed yet)', () => {
    const out = finalizeTrailingAnswer([activeWork(), streamingAnswer('   ')]);
    expect(out).toHaveLength(1);
    expect(out[0].kind).toBe('work');
  });

  it('is a no-op when the tail is not a streaming assistant bubble', () => {
    const rows = [activeWork()];
    expect(finalizeTrailingAnswer(rows)).toBe(rows);
  });

  it('the /stop composition leaves the reply a bubble below a Cancelled block', () => {
    // The exact shape the live cancellation path produces:
    // finalizeTrailingAnswer → closeActiveWork → markLastWorkCancelled.
    const rows = [activeWork(), streamingAnswer('a b-tree is')];
    const out = markLastWorkCancelled(closeActiveWork(finalizeTrailingAnswer(rows)));
    expect(out).toHaveLength(2);
    expect(out[0].kind).toBe('work');
    expect(out[0].workActive).toBe(false);
    expect(out[0].workCancelled).toBe(true);
    expect(out[1].role).toBe('assistant');
    expect(out[1].streaming).toBe(false);
    expect(out[1].text).toBe('a b-tree is');
  });
});

// Pins the approval labelling of a tool step: the badge tracks the PROMPT
// (`approval_requested` → `approval_resolved`), the verdict rides the step,
// and a call that finishes always retires the badge — including the gate
// timing out, which broadcasts no resolution at all.

describe('work-step approval labelling', () => {
  const withCall = (): TranscriptRow[] =>
    pushToolStartedStep([], 'call-1', 'Bash', 'rm -rf build', true);

  it('badges the step of the tool call the prompt blocks', () => {
    const out = markStepAwaitingApproval(withCall(), 'call-1', 'prompt-9');
    expect(out[0].steps?.[0].awaitingApproval).toBe('prompt-9');
    expect(out[0].steps?.[0].toolStatus).toBe('running');
  });

  it('a resolution clears the badge and labels the step by PROMPT id', () => {
    const asked = markStepAwaitingApproval(withCall(), 'call-1', 'prompt-9');
    const out = resolveStepApproval(asked, 'prompt-9', 'approve_always');
    expect(out[0].steps?.[0].awaitingApproval).toBeUndefined();
    expect(out[0].steps?.[0].approval).toBe('approve_always');
  });

  it('leaves a thread untouched when the resolved prompt is another session\'s', () => {
    // `approval_resolved` is broadcast connection-wide, so every open thread
    // sees every decision. An unrelated one must not mint a new transcript.
    const asked = markStepAwaitingApproval(withCall(), 'call-1', 'prompt-9');
    expect(resolveStepApproval(asked, 'prompt-other', 'deny')).toBe(asked);
  });

  it('an unknown decision string is dropped, not rendered raw', () => {
    const asked = markStepAwaitingApproval(withCall(), 'call-1', 'prompt-9');
    const out = resolveStepApproval(asked, 'prompt-9', 'maybe');
    expect(out[0].steps?.[0].approval).toBeUndefined();
  });

  it('completion carries the persisted decision and retires the badge', () => {
    const asked = markStepAwaitingApproval(withCall(), 'call-1', 'prompt-9');
    const out = applyToolCompletedStep(asked, 'call-1', 'ok', 'removed 1.2 GB', 'approve', true);
    expect(out[0].steps?.[0].awaitingApproval).toBeUndefined();
    expect(out[0].steps?.[0].approval).toBe('approve');
    expect(out[0].steps?.[0].toolStatus).toBe('ok');
  });

  it('a timed-out gate retires the badge on the denial alone (no resolution frame)', () => {
    const asked = markStepAwaitingApproval(withCall(), 'call-1', 'prompt-9');
    const out = applyToolCompletedStep(asked, 'call-1', 'denied', 'denied', 'deny', true);
    expect(out[0].steps?.[0].awaitingApproval).toBeUndefined();
    expect(out[0].steps?.[0].toolStatus).toBe('denied');
    expect(out[0].steps?.[0].approval).toBe('deny');
  });

  it('a completion without a decision keeps the one the resolution already set', () => {
    const asked = markStepAwaitingApproval(withCall(), 'call-1', 'prompt-9');
    const resolved = resolveStepApproval(asked, 'prompt-9', 'approve');
    const out = applyToolCompletedStep(resolved, 'call-1', 'ok', 'ok', undefined, true);
    expect(out[0].steps?.[0].approval).toBe('approve');
  });

  it('an ungated call carries no label at all', () => {
    const out = applyToolCompletedStep(withCall(), 'call-1', 'ok', '3 files', undefined, true);
    expect(out[0].steps?.[0].approval).toBeUndefined();
    expect(out[0].steps?.[0].awaitingApproval).toBeUndefined();
  });
});

// The server reconstructs a turn's work block per page window, so a turn longer
// than one page comes back as two `w<ordinal>` items — the older page's
// tail-of-turn half and the current thread's head-of-turn half. Scrolling up
// (or a mid-turn forward sync) puts them adjacent; `foldAdjacentWork` fuses
// them so one turn stays one card. Mirrors the iOS transcript's join.
describe('foldAdjacentWork — a turn cut by a page boundary is still one turn', () => {
  // Absolute-ms timings off one turn: user prompt at TURN_START, the older
  // page's trailing flush at FIRST_END, the newer page's half opening at
  // SECOND_START and closing on the answer at TURN_END. Neither half's own
  // span is the turn's.
  const TURN_START = 1784173283804;
  const FIRST_END = 1784173863833;
  const SECOND_START = 1784173872143;
  const TURN_END = 1784174854449;
  const wkey = (ord: number): string => `row-s1-w${ord}`;
  // `complete` mirrors the server's `turn_complete`: a cut-off head is `false`
  // (its turn continues into the next page), a whole block `true`.
  const half = (
    ord: number,
    label: string,
    started: number,
    ended: number,
    complete: boolean,
  ): TranscriptRow => ({
    key: wkey(ord),
    role: 'system',
    text: '',
    kind: 'work',
    workActive: false,
    workComplete: complete,
    workStartedAt: started,
    workEndedAt: ended,
    steps: [{ key: `${wkey(ord)}-0`, kind: 'tool', tool: 'fetch', toolLabel: label, toolStatus: 'ok' }],
  });
  // The older page's cut-off head, and the newer page's half that closes on the answer.
  const first = (): TranscriptRow => half(3, 'Fetch(a)', TURN_START, FIRST_END, false);
  const second = (): TranscriptRow => half(43, 'Fetch(b)', SECOND_START, TURN_END, true);

  it('spans the whole turn — neither half owns the turn duration', () => {
    const [row] = foldAdjacentWork([first(), second()]);
    expect(row.workStartedAt).toBe(TURN_START);
    expect(row.workEndedAt).toBe(TURN_END);
  });

  it('keeps one card, the earlier-ordinal key, and every step from both halves', () => {
    const out = foldAdjacentWork([first(), second()]);
    expect(out).toHaveLength(1);
    expect(out[0].key).toBe(wkey(3));
    expect(out[0].steps).toHaveLength(2);
  });

  it('stays live when the newer half is still running, anchored at the turn start', () => {
    const live: TranscriptRow = { ...second(), workActive: true, workEndedAt: undefined };
    const [row] = foldAdjacentWork([first(), live]);
    expect(row.workActive).toBe(true);
    expect(row.workStartedAt).toBe(TURN_START);
  });

  it('carries a cancel flag from either half', () => {
    const [row] = foldAdjacentWork([{ ...first(), workCancelled: true }, second()]);
    expect(row.workCancelled).toBe(true);
  });

  it("leaves a healthy thread alone — a turn's block is separated by its answer", () => {
    const rows: TranscriptRow[] = [
      first(),
      { key: 'row-s1-m91', role: 'assistant', text: 'done' },
      second(),
    ];
    expect(foldAdjacentWork(rows)).toHaveLength(3);
  });

  it('is idempotent, so it is safe to run at every seam', () => {
    const once = foldAdjacentWork([first(), second()]);
    expect(foldAdjacentWork(once)).toEqual(once);
  });

  it('collapses a run of three halves (a turn spanning three pages)', () => {
    // Two cut-off halves then the closing half: fold chains until the whole half arrives.
    const head = half(3, 'Fetch(a)', TURN_START, FIRST_END, false);
    const middle = half(43, 'Fetch(b)', SECOND_START, SECOND_START + 1_000, false);
    const tail = half(80, 'Fetch(c)', SECOND_START + 1_000, TURN_END, true);
    const out = foldAdjacentWork([head, middle, tail]);
    expect(out).toHaveLength(1);
    expect(out[0]).toMatchObject({ key: wkey(3), workStartedAt: TURN_START, workEndedAt: TURN_END, workComplete: true });
    expect(out[0].steps).toHaveLength(3);
  });

  it('dedups a tool step shared across halves by call id (start + result straddled the seam)', () => {
    const a: TranscriptRow = {
      ...first(),
      steps: [{ key: 'a0', kind: 'tool', toolCallId: 'c9', tool: 'bash', toolStatus: 'running' }],
    };
    const b: TranscriptRow = {
      ...second(),
      steps: [{ key: 'b0', kind: 'tool', toolCallId: 'c9', tool: 'bash', toolStatus: 'running' }],
    };
    expect(foldAdjacentWork([a, b])[0].steps).toHaveLength(1);
  });

  it('reconciles a live block with its own reconstruction rather than spanning it', () => {
    // A live block's key isn't `w<ordinal>`, so it's one span in two forms —
    // adopt the server's timing, keep the live key + active state.
    const live: TranscriptRow = {
      key: 'work-999-0',
      role: 'system',
      text: '',
      kind: 'work',
      workActive: true,
      workStartedAt: 999,
      steps: [{ key: 'l0', kind: 'tool', toolCallId: 'c1', tool: 'bash', toolStatus: 'running' }],
    };
    const recon: TranscriptRow = { ...second(), workStartedAt: SECOND_START, workEndedAt: SECOND_START + 4_000 };
    const [row] = foldAdjacentWork([live, recon]);
    expect(row).toMatchObject({
      key: 'work-999-0',
      workActive: true,
      workStartedAt: SECOND_START,
      workEndedAt: SECOND_START + 4_000,
    });
  });

  it('reconciles the same block re-delivered (one key, one span, no doubled steps)', () => {
    const [row] = foldAdjacentWork([first(), first()]);
    expect(row).toMatchObject({ key: wkey(3), workStartedAt: TURN_START, workEndedAt: FIRST_END });
    expect(row.steps).toHaveLength(1);
  });

  it('does NOT fuse a completed turn with the following one (two turns, two cards)', () => {
    // The dangerous edge: a whole turn (its empty final reply left no bubble)
    // ends the older page, and the newer page leads with a DIFFERENT turn's work
    // (a cron / subagent fire). Ordinals differ, but `turn_complete` keeps them
    // apart — a purely positional fold would have wrongly merged them.
    const completeHead: TranscriptRow = { ...first(), workComplete: true };
    expect(foldAdjacentWork([completeHead, second()])).toHaveLength(2);
  });

  it('declines to fuse when completeness is unknown (older server) — degrades to two blocks', () => {
    const unknownHead: TranscriptRow = { ...first(), workComplete: undefined };
    expect(foldAdjacentWork([unknownHead, second()])).toHaveLength(2);
  });
});
