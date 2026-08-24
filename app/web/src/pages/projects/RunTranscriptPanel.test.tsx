import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter } from 'react-router-dom';

import { installScrollBox } from '../../test/domGaps';
import { RunTranscriptPanel } from './RunTranscriptPanel';
import { RUN_POLL_MS, type IssueRun } from './boardModel';
import type { Outcome, RunTranscript } from './api';

const SESSION = '01SESSION';

const api = vi.hoisted(() => ({ fetchRunTranscript: vi.fn() }));
vi.mock('./api', () => ({ fetchRunTranscript: api.fetchRunTranscript }));

const auth = vi.hoisted(() => ({ logout: vi.fn() }));
vi.mock('../../api/auth', () => ({
  useAdminClient: () => ({}),
  useAuth: () => ({ baseUrl: 'http://board.test', token: null, logout: auth.logout }),
}));

function run(attempt: number, overrides: Partial<IssueRun> = {}): IssueRun {
  return {
    number: 7,
    attempt,
    agent_id: 'dev-1',
    status: 'done',
    trigger: 'started',
    created_at_ms: 0,
    started_at_ms: attempt * 10_000,
    settled_at_ms: attempt * 10_000 + 5_000,
    session_id: SESSION,
    ...overrides,
  };
}

type Item = RunTranscript['transcript'][number];

function message(ordinal: number, role: string, text: string, atMs: number): Item {
  return {
    id: `m${String(ordinal)}`,
    kind: 'message',
    role,
    text,
    created_at: new Date(atMs).toISOString(),
    ordinal,
    has_attachments: false,
    platform_msg_id: '',
  };
}

function workItem(ordinal: number, atMs: number): Item {
  return {
    id: `w${String(ordinal)}`,
    kind: 'work',
    role: 'system',
    text: '',
    created_at: new Date(atMs).toISOString(),
    ordinal,
    has_attachments: false,
    platform_msg_id: '',
    work_started_at: new Date(atMs).toISOString(),
    work_ended_at: new Date(atMs + 4_000).toISOString(),
    turn_complete: true,
    steps: [{ kind: 'tool', tool: 'Bash', call_id: 'c1', tool_status: 'ok' }],
  };
}

function page(items: Item[], overrides: Partial<RunTranscript> = {}): Outcome<RunTranscript> {
  const ordinals = items.map((item) => item.ordinal ?? 0);
  return {
    kind: 'ok',
    value: {
      session_id: SESSION,
      created_at: '2026-01-01T00:00:00Z',
      last_active: '2026-01-01T00:00:00Z',
      hidden: false,
      transcript: items,
      has_more: false,
      oldest_ordinal: Math.min(...ordinals),
      newest_ordinal: Math.max(...ordinals),
      compaction_points: [],
      ...overrides,
    },
  };
}

/// One run of one card: the brief it was handed, the work it did, its reply.
const ONE_RUN = [
  message(1, 'user', '[issue #7] Wire the retry', 10_000),
  workItem(2, 11_000),
  message(3, 'assistant', 'retry now backs off', 14_000),
];

function panel(runs: IssueRun[], attempt: number) {
  return (
    <MemoryRouter>
      <RunTranscriptPanel
        projectId="01JPROJECT"
        number={7}
        attempt={attempt}
        sessionId={SESSION}
        runs={runs}
        agents={[]}
        portrait={() => null}
        onClose={vi.fn()}
      />
    </MemoryRouter>
  );
}

function mount(runs: IssueRun[], attempt = 1) {
  const view = render(panel(runs, attempt));
  return {
    ...view,
    settle: (next: IssueRun[], nextAttempt = attempt) => {
      view.rerender(panel(next, nextAttempt));
    },
  };
}

describe('RunTranscriptPanel', () => {
  beforeEach(() => {
    api.fetchRunTranscript.mockReset();
    auth.logout.mockReset();
    // jsdom reports every box as zero-height, which IS the shape these tests
    // want — a thread too short to scroll. Installed per describe because the
    // helper patches `Element.prototype` for the whole file, and the paging
    // suite below needs a pane taller than nothing.
    installScrollBox({ scrollHeight: 0, clientHeight: 0 });
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('reads a run as the conversation it was', async () => {
    api.fetchRunTranscript.mockResolvedValue(page(ONE_RUN));
    mount([run(1)]);

    expect(await screen.findByText('[issue #7] Wire the retry')).toBeInTheDocument();
    expect(screen.getByText('retry now backs off')).toBeInTheDocument();
    // The work between the two is a card, not a wall of steps: it says how
    // long it took and opens on a press.
    const worked = screen.getByRole('button', { name: /worked/i });
    await userEvent.click(worked);
    expect(screen.getByText('Bash')).toBeInTheDocument();
  });

  it('renders the brief as the markdown it was written in', async () => {
    // The brief is the card's description, straight out of the card's markdown
    // editor — the panel would otherwise open on a wall of `**` and backticks.
    api.fetchRunTranscript.mockResolvedValue(
      page([
        message(1, 'user', '**fix** the `retry`', 10_000),
        message(2, 'assistant', 'done', 12_000),
      ]),
    );
    mount([run(1)]);

    const strong = await screen.findByText('fix');
    expect(strong.tagName).toBe('STRONG');
    expect(screen.getByText('retry').tagName).toBe('CODE');
  });

  it('keeps the lines of a pasted log apart', async () => {
    // Markdown folds single newlines into one paragraph, which would reflow a
    // log into a wall; a user row renders with `breaks`, so a newline stays a
    // line. This is the whole price of rendering user text as markdown.
    api.fetchRunTranscript.mockResolvedValue(
      page([message(1, 'user', 'line one\nline two', 10_000)]),
    );
    mount([run(1)]);

    const first = await screen.findByText(/line one/);
    expect(first.querySelector('br')).not.toBeNull();
  });

  it('carries the panel’s own type scale', async () => {
    // The size lives in `index.css` under `.run-thread`, so this is the hook
    // it hangs on: lose the class and the thread silently reverts to the
    // chat's reading size, which nothing else on this page would notice.
    api.fetchRunTranscript.mockResolvedValue(page(ONE_RUN));
    mount([run(1)]);

    const reply = await screen.findByText('retry now backs off');
    expect(reply.closest('.run-thread')).not.toBeNull();
  });

  it('says how a run ended when it ended without saying anything', async () => {
    // A turn that dies mid-work leaves a transcript that simply stops. The
    // chat's own guess for that shape is "Cancelled" — right there, where the
    // only way to reach it is `/stop`, and wrong here, where a run gets there
    // by being rate-limited or losing its process.
    api.fetchRunTranscript.mockResolvedValue(page([message(1, 'user', 'the ask', 10_000), workItem(2, 11_000)]));
    mount([
      run(1, { status: 'failed', error: 'LLM rate limited: 429 usage_limit_reached' }),
    ]);

    expect(await screen.findByText(/rate limited/)).toBeInTheDocument();
    expect(screen.queryByText(/^cancelled$/i)).toBeNull();
  });

  it('says nothing extra when the run signed off with a reply', async () => {
    api.fetchRunTranscript.mockResolvedValue(page(ONE_RUN));
    mount([run(1)]);

    await screen.findByText('retry now backs off');
    expect(screen.queryByText(/ended without a reply/)).toBeNull();
  });

  it('draws no run seam on a session that holds one run', async () => {
    // The panel's own header already names it; a rule over the first row
    // divides it from nothing, and stuck to the top it floated over the text.
    api.fetchRunTranscript.mockResolvedValue(page(ONE_RUN));
    mount([run(1)]);

    await screen.findByText('retry now backs off');
    expect(screen.queryByText('moved to In Progress')).toBeNull();
  });

  it('names each attempt, because a session holds every run one agent made', async () => {
    // Run #3's icon opens a page that also holds #1 and #2 — without a mark
    // per run it reads as one turn that keeps starting over.
    api.fetchRunTranscript.mockResolvedValue(
      page([
        message(1, 'user', 'first ask', 10_000),
        message(2, 'assistant', 'first answer', 12_000),
        message(3, 'user', 'second ask', 20_000),
        message(4, 'assistant', 'second answer', 22_000),
      ]),
    );
    mount([run(2), run(1)], 2);

    await screen.findByText('first ask');
    const headers = await screen.findAllByText('moved to In Progress');
    expect(headers).toHaveLength(2);
    expect(screen.getByText('second ask')).toBeInTheDocument();
  });

  it('closes the block and fetches the last word when the run settles', async () => {
    // The tail block is force-opened while the ledger says the run is going;
    // the moment it settles, that must come back off — and the reply that
    // ended it landed after the final poll, so one more read is owed.
    vi.useFakeTimers();
    const running = [
      workItem(2, 11_000),
    ];
    api.fetchRunTranscript.mockResolvedValue(page([message(1, 'user', 'the ask', 10_000), ...running]));
    const view = mount([run(1, { status: 'running', settled_at_ms: undefined })]);

    await vi.advanceTimersByTimeAsync(RUN_POLL_MS);
    expect(screen.getByText(/working/i)).toBeInTheDocument();

    api.fetchRunTranscript.mockClear();
    api.fetchRunTranscript.mockResolvedValue(page(ONE_RUN));
    view.settle([run(1, { status: 'done' })]);
    await vi.advanceTimersByTimeAsync(0);

    // One last read, and the block is a finished `Worked …` again.
    expect(api.fetchRunTranscript).toHaveBeenCalledTimes(1);
    expect(screen.queryByText(/working/i)).toBeNull();
    expect(screen.getByText('retry now backs off')).toBeInTheDocument();
  });

  it('keeps re-reading a live run, and reads a settled one once', async () => {
    vi.useFakeTimers();
    api.fetchRunTranscript.mockResolvedValue(page(ONE_RUN));

    const settled = mount([run(1, { status: 'done' })]);
    await vi.advanceTimersByTimeAsync(RUN_POLL_MS * 3);
    expect(api.fetchRunTranscript).toHaveBeenCalledTimes(1);
    settled.unmount();

    api.fetchRunTranscript.mockClear();
    mount([run(1, { status: 'running', settled_at_ms: undefined })]);
    await vi.advanceTimersByTimeAsync(RUN_POLL_MS * 3);
    expect(api.fetchRunTranscript.mock.calls.length).toBeGreaterThan(1);
  });

  it('pages back on its own for a thread too short to scroll', async () => {
    // A run folds to a couple of `Worked …` cards, so the thread is often
    // shorter than the pane — and a page you cannot scroll can never reach
    // the top. jsdom reports every box as zero-height, which is that shape.
    api.fetchRunTranscript.mockResolvedValueOnce(page(ONE_RUN, { has_more: true }));
    api.fetchRunTranscript.mockResolvedValueOnce(
      page([message(0, 'user', 'the earlier ask', 1_000)], { has_more: false }),
    );
    mount([run(1)]);

    expect(await screen.findByText('the earlier ask')).toBeInTheDocument();
    // …and it stops once there is nothing above: exactly two reads.
    await waitFor(() => {
      expect(api.fetchRunTranscript).toHaveBeenCalledTimes(2);
    });
    expect(api.fetchRunTranscript.mock.calls[1][4]).toBe(1);
  });

  it('draws the compaction seam the chat draws, from the same boundaries', async () => {
    api.fetchRunTranscript.mockResolvedValue(
      page(ONE_RUN, { compaction_points: [{ ordinal: 3, at: '2026-01-01T00:00:30Z' }] }),
    );
    mount([run(1)]);

    expect(await screen.findByText(/compacted/i)).toBeInTheDocument();
  });

  it('reports a failed backfill above the thread, not over it', async () => {
    // The thread on screen is still valid; swapping it for an error box is a
    // worse answer than the box, and on a settled run nothing would clear it.
    api.fetchRunTranscript.mockResolvedValueOnce(page(ONE_RUN, { has_more: true }));
    api.fetchRunTranscript.mockResolvedValueOnce({ kind: 'failed', message: 'HTTP Error 500' });
    mount([run(1)]);

    expect(await screen.findByText('HTTP Error 500')).toBeInTheDocument();
    expect(screen.getByText('retry now backs off')).toBeInTheDocument();
  });

  it('does not re-read the page when a second run of the same session is picked', async () => {
    // Every run of one session resolves to the same page on the server, so a
    // refetch could only repaint what is already there — after blanking it.
    api.fetchRunTranscript.mockResolvedValue(page(ONE_RUN));
    const view = mount([run(1), run(2)], 1);

    await screen.findByText('retry now backs off');
    api.fetchRunTranscript.mockClear();
    view.settle([run(1), run(2)], 2);

    expect(api.fetchRunTranscript).not.toHaveBeenCalled();
    expect(screen.getByText('retry now backs off')).toBeInTheDocument();
    expect(screen.getByRole('region', { name: /run #2 conversation/i })).toBeInTheDocument();
  });

  it('says so when the read that should have caught the run’s last words fails', async () => {
    // The settle read is the LAST one — the poll is torn down by then — so a
    // blip there is not a blip: it is the closing words missing for good, and
    // silence would present a truncated conversation as the whole of it.
    vi.useFakeTimers();
    api.fetchRunTranscript.mockResolvedValue(page([message(1, 'user', 'the ask', 10_000)]));
    const view = mount([run(1, { status: 'running', settled_at_ms: undefined })]);
    await vi.advanceTimersByTimeAsync(RUN_POLL_MS);

    api.fetchRunTranscript.mockResolvedValue({ kind: 'failed', message: 'HTTP Error 500' });
    view.settle([run(1, { status: 'done' })]);
    await vi.advanceTimersByTimeAsync(0);

    expect(screen.getByText(/could not read how this run ended/i)).toBeInTheDocument();
    // …and what it already had is still on screen, not swapped for a box.
    expect(screen.getByText('the ask')).toBeInTheDocument();
  });

  it('does not lock paging back off because the first read blipped', async () => {
    // `has_more` used to be taken only from the FIRST read, which a failure
    // never reaches — leaving the panel unable to page back for the rest of
    // its life with rows still above the window.
    vi.useFakeTimers();
    api.fetchRunTranscript.mockResolvedValueOnce({ kind: 'failed', message: 'HTTP Error 500' });
    api.fetchRunTranscript.mockResolvedValue(page(ONE_RUN, { has_more: true }));
    mount([run(1, { status: 'running', settled_at_ms: undefined })]);
    await vi.advanceTimersByTimeAsync(0);
    expect(screen.getByText('HTTP Error 500')).toBeInTheDocument();

    await vi.advanceTimersByTimeAsync(RUN_POLL_MS);
    await vi.advanceTimersByTimeAsync(0);
    expect(screen.queryByText('HTTP Error 500')).toBeNull();
    // A backfill followed, which only `has_more` could have started.
    const paged = api.fetchRunTranscript.mock.calls.filter((call) => call[4] !== null);
    expect(paged.length).toBeGreaterThan(0);
  });

  it('says what went wrong rather than showing an empty conversation', async () => {
    api.fetchRunTranscript.mockResolvedValue({ kind: 'failed', message: 'HTTP Error 500' });
    mount([run(1)]);

    expect(await screen.findByText('HTTP Error 500')).toBeInTheDocument();
  });

  it('hands an expired token back to the login screen', async () => {
    api.fetchRunTranscript.mockResolvedValue({ kind: 'unauthorized' });
    mount([run(1)]);

    await waitFor(() => {
      expect(auth.logout).toHaveBeenCalled();
    });
  });

  it('reads a run without offering to drive one', async () => {
    // The point of the panel: no composer, and no approve/deny. A prompt
    // waiting on an answer is answered on the card's own timeline, which is
    // the pane directly behind this one.
    api.fetchRunTranscript.mockResolvedValue(page(ONE_RUN));
    mount([run(1)]);

    await screen.findByText('retry now backs off');
    expect(screen.queryByRole('textbox')).toBeNull();
    for (const name of [/approve/i, /deny/i, /send/i]) {
      expect(screen.queryByRole('button', { name })).toBeNull();
    }
  });

  it('keeps the trace viewer one press away', async () => {
    // Spans, per-call tokens and the context window live only there.
    api.fetchRunTranscript.mockResolvedValue(page(ONE_RUN));
    mount([run(1)]);

    await screen.findByText('retry now backs off');
    expect(screen.getByRole('link', { name: /full trace/i })).toHaveAttribute(
      'href',
      `/traces/${SESSION}`,
    );
  });
});

describe('RunTranscriptPanel paging back', () => {
  // jsdom runs no layout and ships no ResizeObserver, so the bottom-edge hold
  // is inert here unless both are supplied — and it is the hold that made
  // "Load earlier" scroll straight past everything it had just fetched.
  const grow: (() => void)[] = [];

  beforeEach(() => {
    api.fetchRunTranscript.mockReset();
    installScrollBox({ scrollHeight: 2000, clientHeight: 400 });
    grow.length = 0;
    (globalThis as unknown as { ResizeObserver: unknown }).ResizeObserver = class {
      constructor(private readonly cb: () => void) {
        grow.push(() => {
          this.cb();
        });
      }
      observe() {}
      unobserve() {}
      disconnect() {}
    };
  });

  function scroller() {
    const region = screen.getByRole('region', { name: /run #1 conversation/i });
    const box = region.querySelector('.overflow-y-auto');
    if (box === null) throw new Error('no scroller');
    return box;
  }

  it('opens on the run whose icon was pressed, not at the foot of the session', async () => {
    // A session holds every attempt, so a page that always opened at the
    // bottom would make the reader hunt for the one they asked for. jsdom
    // lays nothing out, so the two offsets the jump reads are supplied here.
    const SLICE_TOP = 800;
    Object.defineProperty(HTMLElement.prototype, 'offsetTop', {
      configurable: true,
      get(this: HTMLElement) {
        return this.className.includes('overflow-y-auto') ? 0 : SLICE_TOP;
      },
    });
    api.fetchRunTranscript.mockResolvedValue(
      page([
        message(1, 'user', 'first ask', 10_000),
        message(2, 'assistant', 'first answer', 12_000),
        message(3, 'user', 'second ask', 20_000),
        message(4, 'assistant', 'second answer', 22_000),
      ]),
    );
    mount([run(2), run(1)], 1);

    await screen.findByText('first ask');
    expect(scroller().scrollTop).toBe(SLICE_TOP);
  });

  it('stays at the foot when the run pressed is the one still working', async () => {
    // Its foot is where the work is arriving; jumping to its head would take
    // the reader away from the thing they opened the panel to watch.
    Object.defineProperty(HTMLElement.prototype, 'offsetTop', {
      configurable: true,
      get: () => 800,
    });
    api.fetchRunTranscript.mockResolvedValue(page(ONE_RUN));
    mount([run(1, { status: 'running', settled_at_ms: undefined })]);

    await screen.findByText('retry now backs off');
    expect(scroller().scrollTop).toBe(0);
  });

  it('holds the reader where they were instead of snapping to the foot', async () => {
    api.fetchRunTranscript.mockResolvedValueOnce(page(ONE_RUN, { has_more: true }));
    mount([run(1)]);

    await screen.findByText('retry now backs off');
    api.fetchRunTranscript.mockResolvedValueOnce(
      page([message(0, 'user', 'the earlier ask', 1_000)], { has_more: false }),
    );
    // The pane is taller than its content here, so nothing auto-loads: the
    // reader reaching the top is what asks.
    fireEvent.scroll(scroller());
    await screen.findByText('the earlier ask');

    // The prepend grows the observed content box. Pinned, the hold answers
    // that by parking the reader at the very bottom — past the rows the
    // press existed to fetch.
    for (const fire of grow) fire();
    expect(scroller().scrollTop).not.toBe(2000);
  });
});
