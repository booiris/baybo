import { describe, it, expect } from 'vitest';

import {
  workBlockDisplay,
  formatWorkedLabel,
  closeActiveWork,
  isStopCommand,
  isStopCancellationNotice,
  markLastWorkCancelled,
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
});

describe('formatWorkedLabel — never "0s", "Cancelled" for /stop', () => {
  it('renders just "Worked" for a sub-second turn', () => {
    expect(formatWorkedLabel(0)).toBe('Worked');
  });

  it('renders the whole-second duration once it reaches 1s', () => {
    expect(formatWorkedLabel(1)).toBe('Worked 1s');
    expect(formatWorkedLabel(42)).toBe('Worked 42s');
  });

  it('labels a cancelled turn distinctly, still never "0s"', () => {
    expect(formatWorkedLabel(0, true)).toBe('Cancelled');
    expect(formatWorkedLabel(7, true)).toBe('Cancelled · Worked 7s');
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
});
