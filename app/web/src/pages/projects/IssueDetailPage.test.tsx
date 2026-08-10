import { beforeEach, describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, Route, Routes } from 'react-router-dom';

import { IssueDetailPage } from './IssueDetailPage';
import type { Issue, IssueRun } from './boardModel';


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

let current: Issue = issue();
let currentRuns: IssueRun[] = RUNS;

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
      return { data: { items: [] }, error: undefined, response: ok };
    }
    if (path === '/v1/projects/{project_id}/agents') {
      return { data: { items: TEAM }, error: undefined, response: ok };
    }
    throw new Error(`unexpected GET ${path}`);
  }),
  POST: vi.fn(async () => ({ data: RUNS[0], error: undefined, response: ok })),
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

const auth = { logout: vi.fn() };
vi.mock('../../api/auth', () => ({
  useAdminClient: () => client,
  useAuth: () => auth,
}));

function renderIssue(row: Issue, runs: IssueRun[] = RUNS) {
  current = row;
  currentRuns = runs;
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
    // Nothing on this page starts a run any more. Work begins by moving the
    // card, putting somebody on it, commenting, a stage barrier, or the
    // board taking it off the top of Todo.
    renderIssue(issue(), [run('failed')]);

    expect(await screen.findByText('failed')).toBeInTheDocument();
    for (const gone of ['Retry', 'Run it now', 'Run it again', 'Start now']) {
      expect(screen.queryByRole('button', { name: gone })).toBeNull();
    }
    expect(client.POST).not.toHaveBeenCalled();
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

  it('says why the log is empty instead of showing a bare heading', async () => {
    renderIssue(issue(), []);

    expect(await screen.findByText(/No runs yet/)).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Run it now' })).toBeNull();
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
    expect(screen.getByLabelText('Status')).toHaveValue('in_progress');
    expect(screen.getByLabelText('Priority')).toHaveValue('high');
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

  it('copies the branch without a clipboard, rather than throwing at one', async () => {
    // `navigator.clipboard` is absent outside a secure context — which a
    // dashboard on a plain-http LAN address is, and which jsdom is too.
    renderIssue(issue({ branch: 'issue/7-ws-reconnect' }));

    expect(await screen.findByText('issue/7-ws-reconnect')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'copy' }));
    expect(screen.getByRole('button', { name: 'copy' })).toBeInTheDocument();
  });
});
