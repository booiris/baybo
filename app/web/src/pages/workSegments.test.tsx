import { describe, expect, it } from 'vitest';
import { render } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import {
  WorkBlock,
  segmentWorkSteps,
  workRunLabel,
  type TranscriptRow,
  type WorkStep,
} from './ChatPage';

// A turn renders as a LADDER: one `Worked Xs ›` run per stretch of work, each
// timing itself from the model's previous remark to its next, with the remarks
// between them at answer typography and never folded. `app/ios/web/src/
// WorkBlock.test.tsx` pins the same behaviour on the transcript's
// hand-duplicated renderer.

const T0 = 1_700_000_000_000;
const at = (s: number) => T0 + s * 1000;

const reasoning = (text: string, sec: number): WorkStep => ({
  key: `r:${text}`,
  kind: 'reasoning',
  text,
  at: at(sec),
});
const tool = (id: string, sec: number): WorkStep => ({
  key: `t:${id}`,
  kind: 'tool',
  toolCallId: id,
  tool: 'bash',
  toolLabel: `Bash(${id})`,
  toolStatus: 'ok',
  at: at(sec),
});
const says = (text: string, sec: number): WorkStep => ({
  key: `p:${text}`,
  kind: 'prose',
  text,
  at: at(sec),
});

const block = (steps: WorkStep[], over: Partial<TranscriptRow> = {}): TranscriptRow => ({
  key: 'row-s1-w7',
  role: 'system',
  text: '',
  kind: 'work',
  workActive: false,
  workStartedAt: at(0),
  workEndedAt: at(60),
  steps,
  ...over,
});

// turn start ─12s─ "我先找一下" ─40s─ "找到了" ─8s─ turn end
const LADDER = [
  reasoning('thinking', 2),
  says('我先找一下 fold 在哪', 12),
  tool('c1', 20),
  tool('c2', 40),
  says('找到了', 52),
  tool('c3', 55),
];
const LADDER_BLOCK = block(LADDER, { workStartedAt: at(0), workEndedAt: at(60) });

const spoken = (root: HTMLElement) => [...root.querySelectorAll('.work-said')].map((el) => el.textContent);
const headers = (root: HTMLElement) => [...root.querySelectorAll('button')].map((el) => el.textContent);
const openPanels = (root: HTMLElement) =>
  [...root.querySelectorAll('.grid')].filter((el) => el.className.includes('grid-rows-[1fr]'));

describe('segmentWorkSteps — each run is bounded by the remarks around it', () => {
  it('spans turn-start → first remark → second remark → turn-end', () => {
    const segs = segmentWorkSteps(LADDER, at(0), at(60));
    expect(segs.map((s) => s.kind)).toEqual(['machinery', 'speech', 'machinery', 'speech', 'machinery']);
    const spans = segs
      .filter((s) => s.kind === 'machinery')
      .map((s) => (s.endedAt ?? 0) - (s.startedAt ?? 0));
    expect(spans).toEqual([12_000, 40_000, 8_000]);
    // …and they TILE the turn: the ladder adds up to the whole.
    expect(spans.reduce((a, b) => a + b, 0)).toBe(60_000);
  });

  it('a turn with no narration is one run spanning the whole turn', () => {
    const segs = segmentWorkSteps([reasoning('r', 1), tool('c1', 2)], at(0), at(30));
    expect(segs).toHaveLength(1);
    expect((segs[0].endedAt ?? 0) - (segs[0].startedAt ?? 0)).toBe(30_000);
  });

  it('leaves a span undefined when its boundary step carries no timestamp', () => {
    // A row a gateway predating `ChatWorkStep.at` reconstructed.
    const untimed: WorkStep[] = [
      { key: 'r', kind: 'reasoning', text: 'r' },
      { key: 'p', kind: 'prose', text: '说点什么' },
      { key: 't', kind: 'tool', toolCallId: 'c1', toolStatus: 'ok' },
    ];
    const segs = segmentWorkSteps(untimed, at(0), at(30));
    expect(segs[0].endedAt).toBeUndefined();
    expect(segs[2].startedAt).toBeUndefined();
  });
});

describe('workRunLabel — the duration it actually covers, or an honest fallback', () => {
  it('labels a known span', () => {
    expect(workRunLabel({ kind: 'machinery', steps: [], startedAt: at(0), endedAt: at(12) }, false)).toBe(
      'Worked 12s',
    );
  });

  it('falls back to a step count rather than inventing a duration', () => {
    const seg: WorkSegment = { kind: 'machinery', steps: [reasoning('a', 1), tool('c1', 2)] };
    expect(workRunLabel(seg, false)).toBe('2 steps');
    expect(workRunLabel({ kind: 'machinery', steps: [reasoning('a', 1)] }, false)).toBe('1 step');
  });

  it('marks the run the stop landed on', () => {
    expect(workRunLabel({ kind: 'machinery', steps: [], startedAt: at(0), endedAt: at(12) }, true)).toBe(
      'Cancelled · Worked 12s',
    );
  });
});
type WorkSegment = ReturnType<typeof segmentWorkSteps>[number];

describe('WorkBlock — the ladder', () => {
  it('renders one header per stretch of work, each with its own duration', () => {
    const { container } = render(<WorkBlock row={LADDER_BLOCK} />);
    expect(headers(container)).toEqual(['Worked 12s', 'Worked 40s', 'Worked 8s']);
    expect(spoken(container)).toEqual(['我先找一下 fold 在哪', '找到了']);
    // Collapsed: every run's machinery is shut, every remark is on screen.
    expect(openPanels(container)).toHaveLength(0);
  });

  it('opens one run without inserting the others', async () => {
    const user = userEvent.setup();
    const { container } = render(<WorkBlock row={LADDER_BLOCK} />);
    await user.click(container.querySelectorAll('button')[1]);
    expect(openPanels(container)).toHaveLength(1);
    expect(container.textContent).toContain('Bash(c1)');
    // The first run stayed shut — its reasoning is not revealed.
    expect(openPanels(container)[0].textContent).not.toContain('thinking');
  });

  it('only the LAST run of a live turn reads as running', () => {
    const { container } = render(<WorkBlock row={block(LADDER, { workActive: true })} />);
    const labels = headers(container);
    expect(labels[0]).toBe('Worked 12s');
    expect(labels[1]).toBe('Worked 40s');
    expect(labels[2]).toContain('Working');
  });

  it('a turn with no narration is a single run — the common shape', () => {
    const { container } = render(<WorkBlock row={block([reasoning('r', 1), tool('c1', 2)])} />);
    expect(headers(container)).toHaveLength(1);
    expect(spoken(container)).toEqual([]);
  });

  it('a block of pure narration has no header at all', () => {
    const { container } = render(<WorkBlock row={block([says('就这样', 5)])} />);
    expect(headers(container)).toEqual([]);
    expect(spoken(container)).toEqual(['就这样']);
  });

  it('marks only the run the stop landed on as cancelled', () => {
    const { container } = render(<WorkBlock row={block(LADDER, { workCancelled: true })} />);
    expect(headers(container)).toEqual(['Worked 12s', 'Worked 40s', 'Cancelled · Worked 8s']);
  });

  it('still renders nothing for a finished, stepless block', () => {
    const { container } = render(<WorkBlock row={block([])} />);
    expect(container.firstChild).toBeNull();
  });

  it('keeps the live affordance for a turn that has produced nothing yet', () => {
    const { container } = render(<WorkBlock row={block([], { workActive: true })} />);
    expect(headers(container)[0]).toContain('Working');
  });
});
