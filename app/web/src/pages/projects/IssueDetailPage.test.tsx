import { beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, Route, Routes } from 'react-router-dom';

import { installScrollBox } from '../../test/domGaps';
import { IssueDetailPage } from './IssueDetailPage';
import type { Issue, IssueRun } from './boardModel';
import type { IssueEvent } from './timelineModel';


const PROJECT_ID = '01JPROJECT';

function issue(overrides: Partial<Issue> = {}): Issue {
  return {
    number: 7,
    project_id: PROJECT_ID,
    title: 'Wire the retry',
    description: '',
    status: 'in_progress',
    priority: 'none',
    position: 0,
    stage: 0,
    assignee: 'dev-1',
    created_at_ms: 0,
    updated_at_ms: 0,
    unread: 0,
    last_run_failed: false,
    pinned: false,
    ...overrides,
  };
}

function run(status: IssueRun['status']): IssueRun {
  return {
    number: 7,
    attempt: 1,
    agent_id: 'dev-1',
    status,
    trigger: 'started',
    created_at_ms: 0,
  };
}

const RUNS: IssueRun[] = [run('failed')];

const TEAM = [
  {
    id: 'dev-1',
    handle: 'dev-1',
    name: 'Dev',
    description: '',
    framework: 'baybo' as const,
    lead: false,
    created_at_ms: 0,
  },
];

const ok = { status: 200, ok: true } as Response;

/// One run's conversation, as the board's transcript route answers it: the
/// brief the card gave it, the work it did, what it said back.
const TRANSCRIPT = {
  session_id: '01SESSION',
  created_at: '2026-01-01T00:00:00Z',
  last_active: '2026-01-01T00:01:00Z',
  hidden: false,
  transcript: [
    {
      id: 'm1',
      kind: 'message' as const,
      role: 'user',
      text: '[issue #7] Wire the retry',
      created_at: '2026-01-01T00:00:00Z',
      ordinal: 1,
      has_attachments: false,
      platform_msg_id: '',
    },
    {
      id: 'w2',
      kind: 'work' as const,
      role: 'system',
      text: '',
      created_at: '2026-01-01T00:00:10Z',
      ordinal: 2,
      has_attachments: false,
      platform_msg_id: '',
      work_started_at: '2026-01-01T00:00:05Z',
      work_ended_at: '2026-01-01T00:00:40Z',
      turn_complete: true,
      steps: [
        { kind: 'tool' as const, tool: 'Bash', call_id: 'c1', tool_status: 'ok' },
      ],
    },
    {
      id: 'm3',
      kind: 'message' as const,
      role: 'assistant',
      text: 'retry now backs off',
      created_at: '2026-01-01T00:00:40Z',
      ordinal: 3,
      has_attachments: false,
      platform_msg_id: '',
    },
  ],
  has_more: false,
  oldest_ordinal: 1,
  newest_ordinal: 3,
  compaction_points: [],
};

function comment(id: string, text: string): IssueEvent {
  return {
    id,
    number: 7,
    actor: { kind: 'user' },
    body: { kind: 'comment', text },
    created_at_ms: 0,
  };
}

let current: Issue = issue();
let currentRuns: IssueRun[] = RUNS;
let currentEvents: IssueEvent[] = [];

const client = {
  GET: vi.fn(async (path: string) => {
    if (path === '/v1/projects/{project_id}/issues/{number}') {
      return { data: current, error: undefined, response: ok };
    }
    if (path === '/v1/projects/{project_id}/issues') {
      return { data: { items: [current] }, error: undefined, response: ok };
    }
    if (path === '/v1/projects/{project_id}/issues/{number}/runs') {
      return { data: { items: currentRuns }, error: undefined, response: ok };
    }
    if (path === '/v1/projects/{project_id}/issues/{number}/events') {
      return { data: { items: currentEvents }, error: undefined, response: ok };
    }
    if (path === '/v1/projects/{project_id}/agents') {
      return { data: { items: TEAM }, error: undefined, response: ok };
    }
    if (path === '/v1/projects/{project_id}/issues/{number}/runs/{attempt}/transcript') {
      return { data: TRANSCRIPT, error: undefined, response: ok };
    }
    throw new Error(`unexpected GET ${path}`);
  }),
  // Typed args rather than `() =>`: the assertions below read
  // `POST.mock.calls` to tell the read stamp apart from a retry, and an
  // argument-less mock types those calls as empty tuples.
  POST: vi.fn(async (path: string, _init?: unknown) => ({
    // A posted comment answers with the entry it created, and the page appends
    // that to the timeline — hand back a run for it and the render throws.
    data: path.endsWith('/comments') ? comment('evt-sent', 'looks right to me') : RUNS[0],
    error: undefined,
    response: ok,
  })),
  PATCH: vi.fn(),
};

vi.mock('./MarkdownEditor', () => ({
  MarkdownEditor: ({
    initialValue,
    onChange,
    onBlur,
    ariaLabel,
    placeholder,
  }: {
    initialValue: string;
    onChange: (markdown: string) => void;
    onBlur?: () => void;
    ariaLabel: string;
    placeholder?: string;
  }) => (
    <textarea
      aria-label={ariaLabel}
      placeholder={placeholder}
      defaultValue={initialValue}
      onChange={(event) => {
        onChange(event.target.value);
      }}
      onBlur={onBlur}
    />
  ),
}));

// No token on purpose: `useBoardStream` opens a real WebSocket the moment it
// has one, and a socket that cannot connect reconnects — each reconnect
// bumping `refreshKey` and refetching the whole board underneath the
// assertions. `baseUrl` is here because the portraits hook reads it.
const auth = { logout: vi.fn(), baseUrl: 'http://board.test', token: null };
vi.mock('../../api/auth', () => ({
  useAdminClient: () => client,
  useAuth: () => auth,
}));

function renderIssue(row: Issue, runs: IssueRun[] = RUNS, events: IssueEvent[] = []) {
  current = row;
  currentRuns = runs;
  currentEvents = events;
  return render(
    <MemoryRouter initialEntries={[`/projects/${PROJECT_ID}/issues/${row.number}`]}>
      <Routes>
        <Route path="/projects/:pid/issues/:num" element={<IssueDetailPage />} />
      </Routes>
    </MemoryRouter>,
  );
}

describe('IssueDetailPage execution log', () => {
  beforeEach(() => {
    client.POST.mockClear();
  });

  it('reports the board’s work rather than commanding it', async () => {
    // Nothing on this page starts a run out of nowhere. Work begins by
    // moving the card, putting somebody on it, commenting, a stage barrier,
    // or the board taking it off the top of Todo — and, on a card whose
    // newest run failed, by pressing Run again.
    renderIssue(issue(), [run('failed')]);

    expect(await screen.findByText('failed')).toBeInTheDocument();
    for (const gone of ['Run again', 'Run it now', 'Start now']) {
      expect(screen.queryByRole('button', { name: gone })).toBeNull();
    }
    // The one POST opening a card makes is the read stamp, which starts
    // nothing.
    for (const [path] of client.POST.mock.calls) {
      expect(path).toBe('/v1/projects/{project_id}/issues/{number}/read');
    }
  });

  it('marks the card read on open, and only this card', async () => {
    renderIssue(issue());

    await screen.findByText('failed');
    const reads = client.POST.mock.calls.filter(
      ([path]) => path === '/v1/projects/{project_id}/issues/{number}/read',
    );
    expect(reads).toHaveLength(1);
    expect(reads[0][1]).toMatchObject({
      params: { path: { project_id: PROJECT_ID, number: 7 } },
    });
  });

  it('offers Run again on a card whose newest run failed, and runs it', async () => {
    // Gated on the same fact the board's badge counts, so the button is
    // present exactly when the thing it clears is being counted.
    renderIssue(issue({ last_run_failed: true }), [run('failed')]);

    const button = await screen.findByRole('button', { name: /run again/i });
    await userEvent.click(button);

    expect(client.POST).toHaveBeenCalledWith(
      '/v1/projects/{project_id}/issues/{number}/runs/retry',
      { params: { path: { project_id: PROJECT_ID, number: 7 } } },
    );
  });

  it('still stops a run that is going', async () => {
    renderIssue(issue(), [run('running')]);

    await userEvent.click(await screen.findByRole('button', { name: 'Cancel' }));
    expect(client.POST).toHaveBeenCalledWith(
      '/v1/projects/{project_id}/issues/{number}/runs/cancel',
      { params: { path: { project_id: PROJECT_ID, number: 7 } } },
    );
  });

  it('offers no stop on a settled run, and no start either', async () => {
    renderIssue(issue(), [run('done')]);

    expect(await screen.findByText('done')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Cancel' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Retry' })).toBeNull();
  });

  it('folds a long log to its newest runs, and says what it is holding', async () => {
    // A card retried all week owned the whole rail and pushed its own totals
    // off the bottom of the pane. What the fold may not do is swallow a
    // failure, so the count of them rides on the toggle.
    const log = Array.from({ length: 12 }, (_, index) => {
      const attempt = 12 - index;
      const status =
        attempt === 12 ? 'running' : attempt === 3 || attempt === 4 ? 'failed' : 'done';
      return { ...run(status), attempt };
    });
    renderIssue(issue(), log);

    const toggle = await screen.findByRole('button', { name: /7 earlier runs/ });
    // A fold that swallows a failure is the one thing this could cost.
    expect(toggle).toHaveTextContent('2 failed');
    expect(screen.getByText('#12')).toBeInTheDocument();
    expect(screen.getByText('#8')).toBeInTheDocument();
    // Not `#7`: that is the card's own number, in the header above.
    expect(screen.queryByText('#6')).toBeNull();
    // The live run keeps its Cancel — folding away the only control that
    // stops a running card is not a saving.
    expect(screen.getByRole('button', { name: 'Cancel' })).toBeInTheDocument();

    await userEvent.click(toggle);
    expect(screen.getByText('#1')).toBeInTheDocument();

    await userEvent.click(screen.getByRole('button', { name: /fold to the newest 5/i }));
    expect(screen.queryByText('#1')).toBeNull();
  });

  it('says why the log is empty instead of showing a bare heading', async () => {
    renderIssue(issue(), []);

    expect(await screen.findByText(/No runs yet/)).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Run it now' })).toBeNull();
  });

  it('opens the run as a conversation rather than navigating to the trace', async () => {
    renderIssue(issue(), [{ ...run('done'), session_id: '01SESSION' }]);

    // The word is gone from the rail but not from the accessibility tree —
    // an unlabelled glyph is a control to nowhere for anybody not looking.
    const icon = await screen.findByRole('button', { name: 'Conversation of run #1' });
    await userEvent.click(icon);

    const panel = await screen.findByRole('region', { name: /run #1 conversation/i });
    // The ask the run was given, and what it said back — the two ends of the
    // flow the trace page makes you assemble yourself.
    expect(panel).toHaveTextContent('[issue #7] Wire the retry');
    expect(panel).toHaveTextContent('retry now backs off');
    // No composer, and nothing to approve: this reads a run, it does not
    // drive one.
    expect(screen.queryByRole('textbox', { name: /message/i })).toBeNull();

    // The trace viewer is still where the spans and the token arithmetic
    // live, so it stays one press away rather than being dropped.
    const trace = screen.getByRole('link', { name: /full trace/i });
    expect(trace).toHaveAttribute('href', '/traces/01SESSION');
  });

  it('swaps the conversation between runs of one session instead of closing it', async () => {
    // The execution log sits OUTSIDE the panel, so the panel's
    // dismiss-on-outside-press used to fire on the way to this very click and
    // shut the thing the press was swapping.
    renderIssue(issue(), [
      { ...run('done'), attempt: 2, session_id: '01SESSION' },
      { ...run('done'), attempt: 1, session_id: '01SESSION' },
    ]);

    await userEvent.click(await screen.findByRole('button', { name: 'Conversation of run #2' }));
    await screen.findByRole('region', { name: /run #2 conversation/i });

    await userEvent.click(screen.getByRole('button', { name: 'Conversation of run #1' }));
    // Past the panel's 180ms slide: the outside-press dismissal is a *timer*,
    // so an assertion made straight after the click passes on the bug too —
    // the panel is still on screen at that instant and gone a moment later.
    await new Promise((resolve) => setTimeout(resolve, 300));
    expect(screen.getByRole('region', { name: /run #1 conversation/i })).toBeInTheDocument();
  });

  it('still lets every other popover on the card close under that press', async () => {
    // The first fix for the swap above stopped the icon's mousedown outright.
    // React delegates to the root container, so that never reached `document`
    // — where every outside-press rule on this page is registered — and one
    // panel's swap silenced all of them.
    renderIssue(issue(), [{ ...run('done'), session_id: '01SESSION' }]);

    await userEvent.click(await screen.findByRole('button', { name: 'More actions' }));
    expect(screen.getByRole('button', { name: 'Block…' })).toBeInTheDocument();

    await userEvent.click(screen.getByRole('button', { name: 'Conversation of run #1' }));
    expect(screen.queryByRole('button', { name: 'Block…' })).toBeNull();
  });

  it('carries no conversation to open on a run nothing ever claimed', async () => {
    renderIssue(issue(), [run('queued')]);

    await screen.findByText('queued');
    expect(screen.queryByRole('button', { name: /conversation of run/i })).toBeNull();
  });

  it('keeps what a run cost on its own line, away from what a run is', async () => {
    // Seven fields in a 340px rail used to `flex-wrap` at whatever point each
    // row's own trigger label ran out of room, so no two rows lined up.
    renderIssue(issue(), [{ ...run('done'), session_id: '01SESSION', cost_micros: 30_000 }]);

    const cost = await screen.findByText('$0.03');
    const metaRow = cost.parentElement;
    expect(metaRow?.className).toContain('justify-end');
    // …and the status chip is on the other line, not sharing the row.
    expect(metaRow?.textContent).not.toContain('done');
  });
});

describe('IssueDetailPage activity scrolling', () => {
  // The chat thread's posture, in the pane a card is answered in. jsdom runs no
  // layout, so the pane is handed one tall enough to scroll.
  const PANE = { scrollHeight: 2000, clientHeight: 400 };
  installScrollBox(PANE);

  it('opens at the foot of the card, and follows the comment you send', async () => {
    renderIssue(issue(), RUNS, [comment('evt-old', 'the first one')]);

    const pane = await screen.findByRole('main');
    await waitFor(() => {
      expect(pane.scrollTop).toBe(PANE.scrollHeight);
    });

    // Read back up the card…
    pane.scrollTop = 0;
    fireEvent.scroll(pane);

    // …and sending still takes you to what you sent, as it does in chat.
    await userEvent.type(
      screen.getByPlaceholderText(/Comment…/),
      'looks right to me{Enter}',
    );
    await waitFor(() => {
      expect(pane.scrollTop).toBe(PANE.scrollHeight);
    });
  });

  it('raises a pill rather than yanking a reader who has scrolled back', async () => {
    renderIssue(issue({ last_run_failed: true }), [run('failed')]);

    const pane = await screen.findByRole('main');
    await waitFor(() => {
      expect(pane.scrollTop).toBe(PANE.scrollHeight);
    });
    pane.scrollTop = 0;
    fireEvent.scroll(pane);

    // An entry lands while they are up there — here through the refetch that
    // Run again triggers, on the board it is an agent's own comment.
    currentEvents = [comment('evt-new', 'ran it again')];
    await userEvent.click(screen.getByRole('button', { name: /run again/i }));

    const pill = await screen.findByRole('button', { name: /new activity/i });
    expect(pane.scrollTop).toBe(0);

    await userEvent.click(pill);
    expect(pane.scrollTop).toBe(PANE.scrollHeight);
    expect(screen.queryByRole('button', { name: /new activity/i })).toBeNull();
  });
});

describe('IssueDetailPage prose', () => {
  it('reads as a heading until you ask to edit it', async () => {
    renderIssue(issue());

    // The title used to be a permanently-live borderless input, which read
    // as an unlabelled text box sitting on top of another one.
    expect(await screen.findByRole('heading', { name: 'Wire the retry' })).toBeInTheDocument();
    expect(screen.queryByLabelText('Issue title')).toBeNull();

    await userEvent.click(screen.getByRole('button', { name: 'Wire the retry' }));
    expect(screen.getByLabelText('Issue title')).toHaveValue('Wire the retry');
  });

  it('names the card in the header too, since it opens below the heading', async () => {
    renderIssue(issue());

    // Landing at the foot of the timeline puts the heading off-screen, so the
    // header is the only place the name shows — and it follows the editor, so
    // a title being typed reads the same in both.
    const heading = await screen.findByRole('button', { name: 'Wire the retry' });
    const header = document.querySelector('header');
    expect(header).not.toBeNull();
    expect(header?.textContent ?? '').toContain('Wire the retry');

    await userEvent.click(heading);
    await userEvent.type(screen.getByLabelText('Issue title'), ' now');
    expect(header?.textContent).toContain('Wire the retry now');
  });

  it('writes the title when the field is left, not when a button is pressed', async () => {
    client.PATCH.mockClear().mockResolvedValue({
      data: { ...issue(), title: 'Wire the retry properly' },
      error: undefined,
      response: ok,
    });
    renderIssue(issue());

    await userEvent.click(await screen.findByRole('button', { name: 'Wire the retry' }));
    // The field opens on the server's own text. It used to open **empty** —
    // the value never reached it — so leaving the field now would write that
    // blank straight over the card.
    expect(screen.getByLabelText('Issue title')).toHaveValue('Wire the retry');

    await userEvent.type(screen.getByLabelText('Issue title'), ' properly{Enter}');

    expect(client.PATCH).toHaveBeenCalledWith(
      '/v1/projects/{project_id}/issues/{number}',
      expect.objectContaining({ body: { title: 'Wire the retry properly' } }),
    );
  });

  it('writes nothing when a field is opened and left alone', async () => {
    client.PATCH.mockClear();
    renderIssue(issue());

    await userEvent.click(await screen.findByRole('button', { name: 'Wire the retry' }));
    await userEvent.tab();

    expect(client.PATCH).not.toHaveBeenCalled();
  });

  it('puts the text back on Escape rather than saving it', async () => {
    client.PATCH.mockClear();
    renderIssue(issue());

    await userEvent.click(await screen.findByRole('button', { name: 'Wire the retry' }));
    await userEvent.type(screen.getByLabelText('Issue title'), ' nonsense{Escape}');

    expect(client.PATCH).not.toHaveBeenCalled();
    expect(screen.getByRole('button', { name: 'Wire the retry' })).toBeInTheDocument();
  });

  it('treats a title erased to nothing as a slip, not an instruction', async () => {
    client.PATCH.mockClear();
    renderIssue(issue());

    await userEvent.click(await screen.findByRole('button', { name: 'Wire the retry' }));
    await userEvent.clear(screen.getByLabelText('Issue title'));
    await userEvent.tab();

    expect(client.PATCH).not.toHaveBeenCalled();
    expect(screen.getByRole('button', { name: 'Wire the retry' })).toBeInTheDocument();
  });

  it('opens at the foot of the timeline, and stays where you put it after', async () => {
    renderIssue(issue());
    await screen.findByRole('heading', { name: 'Wire the retry' });

    // jsdom lays nothing out, so `scrollHeight` is 0 and the assertion is on
    // the *intent*: the pane was anchored once. What matters beyond that is
    // the second half — a board frame invalidates this page constantly, and
    // re-anchoring on each one would yank the page from under a reader.
    const pane = document.querySelector('main');
    expect(pane).not.toBeNull();
    if (pane === null) return;
    pane.scrollTop = 120;

    // A refresh of the same card, as the WS stream provokes.
    current = issue({ title: 'Wire the retry' });
    await userEvent.click(screen.getByRole('button', { name: 'Wire the retry' }));
    await userEvent.keyboard('{Escape}');

    expect(pane.scrollTop).toBe(120);
  });

  it('says a card has no title rather than showing nothing to click', async () => {
    renderIssue(issue({ title: '' }));
    // The heading specifically — the header carries the name too, so a bare
    // text match would find either and prove neither is clickable.
    expect(await screen.findByRole('button', { name: 'Untitled' })).toBeInTheDocument();
  });



  it('writes the description when the field is left', async () => {
    client.PATCH.mockClear().mockResolvedValue({
      data: { ...issue(), description: 'clear on ack' },
      error: undefined,
      response: ok,
    });
    renderIssue(issue({ description: '' }));

    await userEvent.type(await screen.findByLabelText('Issue description'), 'clear on ack');
    await userEvent.tab();

    expect(client.PATCH).toHaveBeenCalledWith(
      '/v1/projects/{project_id}/issues/{number}',
      expect.objectContaining({ body: { description: 'clear on ack' } }),
    );
  });
});

describe('IssueDetailPage rail', () => {
  it('asks before cancelling, and only then says what it costs', async () => {
    client.PATCH.mockClear().mockResolvedValue({
      data: { ...issue(), cancelled_at_ms: 1 },
      error: undefined,
      response: ok,
    });
    renderIssue(issue());

    await userEvent.click(await screen.findByRole('button', { name: 'More actions' }));
    // The menu is two plain actions; the warning is not standing prose under
    // them, where it went unread and made both look alarming.
    expect(screen.queryByText(/keeps its number/, { selector: 'p' })).toBeNull();

    await userEvent.click(screen.getByRole('button', { name: 'Cancel issue' }));
    expect(client.PATCH).not.toHaveBeenCalled();
    expect(screen.getByText(/keeps its number/, { selector: 'p' })).toBeInTheDocument();

    await userEvent.click(screen.getByRole('button', { name: 'Keep' }));
    expect(client.PATCH).not.toHaveBeenCalled();

    await userEvent.click(screen.getByRole('button', { name: 'Cancel issue' }));
    await userEvent.click(screen.getByRole('button', { name: 'Cancel it' }));
    expect(client.PATCH).toHaveBeenCalledWith(
      '/v1/projects/{project_id}/issues/{number}',
      expect.objectContaining({ body: { cancelled: true } }),
    );
  });

  it('asks what a card is blocked on in the panel, never in a browser dialog', async () => {
    const prompt = vi.spyOn(window, 'prompt');
    client.PATCH.mockClear().mockResolvedValue({
      data: { ...issue(), blocked_reason: 'waiting on the staging key' },
      error: undefined,
      response: ok,
    });
    renderIssue(issue());

    await userEvent.click(await screen.findByRole('button', { name: 'More actions' }));
    await userEvent.click(screen.getByRole('button', { name: 'Block…' }));

    // A browser dialog blocks the page and cannot be styled; on a board that
    // streams updates it freezes everything behind it until it is answered.
    expect(prompt).not.toHaveBeenCalled();
    const field = screen.getByLabelText('What is it blocked on?');
    expect(screen.getByRole('button', { name: 'Block' })).toBeDisabled();

    await userEvent.type(field, 'waiting on the staging key');
    await userEvent.click(screen.getByRole('button', { name: 'Block' }));

    expect(client.PATCH).toHaveBeenCalledWith(
      '/v1/projects/{project_id}/issues/{number}',
      expect.objectContaining({ body: { blocked_reason: 'waiting on the staging key' } }),
    );
    prompt.mockRestore();
  });

  it('puts the menu away when you click somewhere else', async () => {
    renderIssue(issue());

    await userEvent.click(await screen.findByRole('button', { name: 'More actions' }));
    expect(screen.getByRole('button', { name: 'Block…' })).toBeInTheDocument();

    await userEvent.click(screen.getByRole('heading', { name: 'Wire the retry' }));
    expect(screen.queryByRole('button', { name: 'Block…' })).toBeNull();
  });

  it('reopens without asking — putting a card back is not destructive', async () => {
    client.PATCH.mockClear().mockResolvedValue({
      data: issue(),
      error: undefined,
      response: ok,
    });
    renderIssue(issue({ cancelled_at_ms: 1 }));

    await userEvent.click(await screen.findByRole('button', { name: 'More actions' }));
    await userEvent.click(screen.getByRole('button', { name: 'Reopen issue' }));

    expect(client.PATCH).toHaveBeenCalledWith(
      '/v1/projects/{project_id}/issues/{number}',
      expect.objectContaining({ body: { cancelled: false } }),
    );
  });

  it('reads as a property table, with every line answered', async () => {
    renderIssue(issue({ priority: 'high' }));

    // The mockup's rail is `label ── value`, not a stack of form widgets;
    // the pickers are laid over the values they set.
    expect((await screen.findByText('Status')).closest('div')?.textContent).toContain(
      'In Progress',
    );
    expect(screen.getByText('Priority').closest('div')?.textContent).toContain('High');
    // Each of the three is a button that names what it is set to, so a
    // screen reader hears the value and not just the field.
    expect(screen.getByLabelText('Status: In Progress')).toBeInTheDocument();
    expect(screen.getByLabelText('Priority: High')).toBeInTheDocument();
    // A card with no parent and no blocker still says so. Leaving the row
    // out entirely made "no parent" and "this page is out of date" look the
    // same.
    expect(screen.getAllByText('—')).toHaveLength(2);
  });

  it('shows what the card has cost before it has cost anything', async () => {
    renderIssue(issue(), []);

    expect(await screen.findByText('cost (all runs)')).toBeInTheDocument();
    expect(screen.getByText('$0.00')).toBeInTheDocument();
    expect(screen.getByText('0 / 0')).toBeInTheDocument();
  });

  it('lines every property value up on the right, picker-wrapped or not', async () => {
    renderIssue(issue());
    await screen.findByText('Status');

    // Three of these rows sit inside a PickerOverlay, which is itself a flex
    // container: without `w-full` the row is sized by its own content and
    // `justify-between` has no slack to push the value into. That is what
    // left Status/Priority/Assignee beside their labels while Parent and
    // Blocked sat flush right. The fixed height is what evens the spacing
    // out, the Assignee row carrying an 18px face and the rest one line.
    for (const label of ['Status', 'Priority', 'Assignee', 'Pinned', 'Parent', 'Blocked']) {
      const row = screen.getByText(label).parentElement;
      expect(row?.className, label).toContain('w-full');
      expect(row?.className, label).toContain('justify-between');
      expect(row?.className, label).toContain('min-h-[26px]');
    }
  });

  it('pins the card from the rail, where a touch screen can always reach it', async () => {
    client.PATCH.mockClear().mockResolvedValue({
      data: { ...issue(), pinned: true },
      error: undefined,
      response: ok,
    });
    renderIssue(issue());

    // The board's own pin is a glyph that appears on hover; this row is on
    // screen whether or not there is a pointer to hover with.
    const pin = await screen.findByRole('button', { name: 'Pin to top' });
    expect(pin).toHaveAttribute('aria-pressed', 'false');
    await userEvent.click(pin);

    expect(client.PATCH).toHaveBeenCalledWith('/v1/projects/{project_id}/issues/{number}', {
      params: { path: { project_id: PROJECT_ID, number: 7 } },
      body: { pinned: true },
    });
    const pinned = await screen.findByRole('button', { name: 'Pinned' });
    expect(pinned).toHaveAttribute('aria-pressed', 'true');
  });

  it('names the branch, and says the worktree will not be there forever', async () => {
    renderIssue(issue({ branch: 'issue/7-ws-reconnect' }));

    // Truncated in the rail, so the full name has to be reachable somewhere.
    expect(await screen.findByText('issue/7-ws-reconnect')).toHaveAttribute(
      'title',
      'issue/7-ws-reconnect',
    );
    expect(screen.getByText(/reclaimed at Done/)).toBeInTheDocument();
  });
});
