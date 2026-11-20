import { afterEach, describe, expect, it, vi } from 'vitest';
import { act, render } from '@testing-library/react';

import {
  WorkSpeechRun,
  WorkStepView,
  isCancelledWorkAt,
  type PendingAsk,
  type TranscriptRow,
  type WorkStep,
} from './ChatPage';

// Which steps inside a work block are MARKDOWN and which are verbatim is a
// contract, not a styling detail: the model writes its reasoning in the same
// markdown as its answer, so rendering it raw leaked `**` to the reader, while a
// status line or a notice is our own string and must never be re-parsed.
// `app/ios/web/src/WorkBlock.test.tsx` holds the iOS transcript to the same
// split — the two `WorkStepView`s are hand-duplicated with nothing else
// gating the drift.

const step = (over: Partial<WorkStep>): WorkStep => ({
  key: 'k1',
  kind: 'reasoning',
  ...over,
});

describe('WorkStepView reasoning', () => {
  it('renders the reasoning trace as markdown, not raw text', () => {
    const { container } = render(
      <WorkStepView step={step({ text: '**要点：**先看配置\n\n再看日志' })} />,
    );
    expect(container.querySelector('strong')?.textContent).toBe('要点：');
    expect(container.textContent).not.toContain('*');
  });

  it('renders ASCII markdown in the trace too', () => {
    const { container } = render(
      <WorkStepView step={step({ text: '- **Key:** value\n- `code` here' })} />,
    );
    expect(container.querySelector('li strong')?.textContent).toBe('Key:');
    expect(container.querySelector('code')?.textContent).toBe('code');
  });

  // The trace used to render inside an `italic` span. Markdown emphasis is
  // invisible inside an italic container, so the styling and the parsing cannot
  // both be there — see `.work-reasoning` in index.css.
  it('keeps emphasis distinguishable — no italic ancestor', () => {
    const { container } = render(<WorkStepView step={step({ text: 'plain *stressed* word' })} />);
    const em = container.querySelector('em');
    expect(em?.textContent).toBe('stressed');
    const italicAncestors: string[] = [];
    for (let node = em?.parentElement ?? null; node !== null; node = node.parentElement) {
      if (node.classList.contains('italic')) italicAncestors.push(node.className);
    }
    expect(italicAncestors).toEqual([]);
  });

  it('survives a step with no text yet', () => {
    const { container } = render(<WorkStepView step={step({ text: undefined })} />);
    expect(container.textContent).toBe('✻');
  });
});

describe('WorkStepView verbatim steps', () => {
  it('leaves a status line unparsed', () => {
    const { container } = render(
      <WorkStepView step={step({ kind: 'status', text: 'reading **config.toml**' })} />,
    );
    expect(container.querySelector('strong')).toBeNull();
    expect(container.textContent).toContain('**config.toml**');
  });

  it('leaves a notice unparsed', () => {
    const { container } = render(
      <WorkStepView
        step={step({ kind: 'notice', noticeLevel: 'warn', text: 'context **compacted**' })}
      />,
    );
    expect(container.querySelector('strong')).toBeNull();
    expect(container.textContent).toContain('**compacted**');
  });

  // The trace is written as short newline-separated lines. CommonMark folds a
  // single newline into a space, which ran the whole trace together the moment
  // it started being parsed; `breaks` is what buys the shape back.
  it('keeps the trace line-broken', () => {
    const trace = render(<WorkStepView step={step({ text: '看配置\n看日志\n看指标' })} />);
    expect(trace.container.querySelectorAll('br')).toHaveLength(2);
  });
});

// Mid-turn narration left the step renderer when the collapse stopped being
// allowed to hide it (see `segmentWorkSteps`), but the typography contract
// moved with it: it is ANSWER text, so it gets the answer's markdown rules —
// notably NOT `breaks`, unlike the reasoning trace above.
describe('WorkSpeechRun — the model\'s own mid-turn words', () => {
  const prose = (text: string): WorkStep => ({ key: `p:${text}`, kind: 'prose', text });

  it('renders narration as markdown', () => {
    const { container } = render(<WorkSpeechRun steps={[prose('**结论：**可以上线')]} />);
    expect(container.querySelector('strong')?.textContent).toBe('结论：');
  });

  it('stays on the paragraph rule — a lone newline does not become a <br>', () => {
    const { container } = render(<WorkSpeechRun steps={[prose('第一行\n第二行')]} />);
    expect(container.querySelector('br')).toBeNull();
  });

  it('renders each paragraph of a coalesced run', () => {
    const { container } = render(<WorkSpeechRun steps={[prose('先看这里'), prose('再看那里')]} />);
    expect(container.textContent).toContain('先看这里');
    expect(container.textContent).toContain('再看那里');
  });

  it('survives a step with no text', () => {
    const { container } = render(
      <WorkSpeechRun steps={[{ key: 'p0', kind: 'prose' }]} />,
    );
    expect(container.textContent).toBe('');
  });
});

// A live trace grows by a chunk per WS frame with nothing pacing it, and each
// frame re-parses the WHOLE accumulated text — quadratic in the trace's length.
// `useSampledText` caps the parse rate; what must NOT regress is that the last
// chunk still lands. Fake timers here: the sampler is wall-clock, not rAF.
describe('WorkStepView reasoning sampling', () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it('paints the first chunk immediately, then settles on the final text', () => {
    vi.useFakeTimers();
    const { container, rerender } = render(<WorkStepView step={step({ text: 'first' })} />);
    expect(container.textContent).toContain('first');

    // A burst inside one sampling window renders once, at the window's end —
    // not once per chunk.
    for (const text of ['first s', 'first se', 'first sec', 'first second']) {
      rerender(<WorkStepView step={step({ text })} />);
    }
    expect(container.textContent).toContain('first');
    expect(container.textContent).not.toContain('second');

    act(() => {
      vi.advanceTimersByTime(200);
    });
    expect(container.textContent).toContain('first second');
  });

  it('does not truncate a trace whose last chunk closes the step', () => {
    vi.useFakeTimers();
    const { container, rerender } = render(<WorkStepView step={step({ text: 'a' })} />);
    rerender(<WorkStepView step={step({ text: 'a **要点：**tail' })} />);
    act(() => {
      vi.advanceTimersByTime(200);
    });
    expect(container.querySelector('strong')?.textContent).toBe('要点：');
    expect(container.textContent).toContain('tail');
  });
});

describe('WorkStepView render-time ask badge & AskStepSummary', () => {
  const approvalAsk: PendingAsk = {
    kind: 'approval',
    askId: 'ask-a1',
    toolCallId: 'call-1',
    sessionId: 's1',
    tool: 'Bash',
    description: null,
    paramsPreview: '{}',
    accesses: [],
  };

  const questionAsk: PendingAsk = {
    kind: 'question',
    askId: 'ask-q1',
    toolCallId: 'call-q1',
    sessionId: 's1',
    questions: [
      {
        header: 'Auth',
        question: 'Which auth?',
        options: [
          { label: 'JWT', description: 'Token' },
          { label: 'Cookie', description: 'Session' },
        ],
        multi_select: false,
      },
    ],
  };

  it('derives waiting-for-approval badge at render time and suppresses spinner', () => {
    const { container } = render(
      <WorkStepView
        step={step({
          kind: 'tool',
          toolCallId: 'call-1',
          tool: 'Bash',
          toolStatus: 'running',
        })}
        pendingAsks={[approvalAsk]}
      />,
    );
    expect(container.textContent).toContain('waiting for approval');
    expect(container.querySelector('.animate-spin')).toBeNull();
  });

  it('derives waiting-for-your-answer badge for pending question and suppresses spinner', () => {
    const { container } = render(
      <WorkStepView
        step={step({
          kind: 'tool',
          toolCallId: 'call-q1',
          tool: 'AskUser',
          toolLabel: 'Auth',
          toolStatus: 'running',
        })}
        pendingAsks={[questionAsk]}
      />,
    );
    expect(container.textContent).toContain('waiting for your answer');
    expect(container.querySelector('.animate-spin')).toBeNull();
  });

  it('renders answered, skipped, and declined AskStepSummary with expandable full options', () => {
    const { container } = render(
      <WorkStepView
        step={step({
          kind: 'tool',
          toolCallId: 'call-q1',
          tool: 'AskUser',
          toolLabel: 'Auth',
          toolStatus: 'ok',
          ask: {
            declined: false,
            cancelled: false,
            questions: [
              {
                header: 'Auth',
                question: 'Which auth strategy?',
                options: [
                  { label: 'JWT', description: 'Stateless' },
                  { label: 'Cookie', description: 'Stateful' },
                ],
                multi_select: false,
                selected: ['JWT'],
                skipped: false,
              },
              {
                header: 'Cache',
                question: 'Need a cache?',
                options: [
                  { label: 'Redis', description: 'In-memory' },
                  { label: 'None', description: 'Skip' },
                ],
                multi_select: false,
                selected: [],
                skipped: true,
              },
            ],
          },
        })}
      />,
    );
    const summary = container.querySelector('summary');
    expect(summary?.textContent).toContain('Asked 2 questions');
    expect(summary?.textContent).toContain('Auth: JWT');
    expect(summary?.textContent).toContain('Cache: skipped');
    expect(summary?.querySelector('em')?.textContent).toBe('skipped');
    // Expanded content includes the full question text and options
    expect(container.textContent).toContain('Which auth strategy?');
    expect(container.textContent).toContain('Stateless');

    const closedCard = (outcome: 'declined' | 'cancelled') => render(
      <WorkStepView
        step={step({
          kind: 'tool',
          toolCallId: 'call-q2',
          tool: 'AskUser',
          toolStatus: 'ok',
          ask: {
            declined: outcome === 'declined',
            cancelled: outcome === 'cancelled',
            questions: [
              {
                header: 'Auth',
                question: 'Which auth strategy?',
                options: [{ label: 'JWT', description: 'Stateless' }],
                multi_select: false,
                selected: [],
                skipped: true,
              },
            ],
          },
        })}
      />,
    );
    for (const outcome of ['declined', 'cancelled'] as const) {
      const { container } = closedCard(outcome);
      expect(container.querySelector('summary')?.textContent).toContain('Asked 1 question');
      expect(container.querySelector('summary em')?.textContent).toBe(outcome);
      expect(container.querySelector('details > div')?.textContent).toContain(outcome);
    }
  });

  it('suppresses CancelledTurnIndicator when the trailing closed block owns a pending ask (post-restart)', () => {
    const trailingWork: TranscriptRow = {
      key: 'row-s1-w4',
      role: 'system',
      text: '',
      kind: 'work',
      workActive: false,
      steps: [
        {
          key: 's0',
          kind: 'tool',
          toolCallId: 'call-q1',
          tool: 'AskUser',
          toolLabel: 'Auth',
        },
      ],
    };
    const deadTurn = { active: false, startedAt: null };
    expect(isCancelledWorkAt([trailingWork], 0, deadTurn, [])).toBe(true);
    expect(isCancelledWorkAt([trailingWork], 0, deadTurn, [questionAsk])).toBe(false);
  });
});
