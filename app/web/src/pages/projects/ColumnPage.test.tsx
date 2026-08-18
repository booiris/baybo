import { beforeEach, describe, expect, it, vi } from 'vitest';
import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, Route, Routes, useLocation } from 'react-router-dom';

import { ColumnPage } from './ColumnPage';
import type { Issue, Project } from './boardModel';

const PROJECT: Project = {
  id: '01JPROJECT',
  name: 'Kanban',
  description: '',
  workdir: '/tmp/kanban',
  max_parallel_issue_runs: 3,
  created_at_ms: 0,
  updated_at_ms: 0,
};

function issue(number: number, overrides: Partial<Issue> = {}): Issue {
  return {
    number,
    project_id: PROJECT.id,
    title: `issue ${number}`,
    description: '',
    status: 'backlog',
    priority: 'none',
    position: 0,
    stage: 0,
    created_at_ms: 0,
    updated_at_ms: 0,
    unread: 0,
    last_run_failed: false,
    pinned: false,
    ...overrides,
  };
}

const ISSUES: Issue[] = [
  issue(1, { title: 'Wire the board', position: 0, assignee: 'dev-1' }),
  issue(2, { title: 'Blocked one', position: 1, blocked_reason: 'waiting on tmux', unread: 2 }),
  issue(3, { title: 'Cancelled one', position: 2, cancelled_at_ms: 111 }),
  issue(4, {
    title: 'Under way',
    status: 'in_progress',
    position: 0,
    priority: 'urgent',
    assignee: 'dev-1',
    // News in a stage that is not on screen — what the tab's dot is for.
    unread: 1,
  }),
];

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
const refused = { status: 500, ok: false } as Response;

/// Swapped per test: the archived variant, the fail-once move.
let projectDto: Project = PROJECT;
let failMoves = 0;

function stubClient() {
  return {
    GET: vi.fn(async (path: string) => {
      if (path === '/v1/projects/{project_id}') {
        return { data: projectDto, error: undefined, response: ok };
      }
      if (path === '/v1/projects/{project_id}/issues') {
        return { data: { items: ISSUES }, error: undefined, response: ok };
      }
      if (path === '/v1/projects/{project_id}/runs') {
        return { data: { items: [] }, error: undefined, response: ok };
      }
      if (path === '/v1/projects/{project_id}/agents') {
        return { data: { items: TEAM }, error: undefined, response: ok };
      }
      if (path === '/v1/projects/attention') {
        return { data: { items: [] }, error: undefined, response: ok };
      }
      if (path === '/v1/llm/models') {
        return {
          data: { items: [{ name: 'model-1' }], default_name: 'model-1' },
          error: undefined,
          response: ok,
        };
      }
      throw new Error(`unexpected GET ${path}`);
    }),
    POST: vi.fn(async (path: string, init: unknown) => {
      if (path === '/v1/projects/{project_id}/issues/{number}/move') {
        if (failMoves > 0) {
          failMoves -= 1;
          return { data: undefined, error: { error: 'the server said no' }, response: refused };
        }
        const body = (init as { body: { status: Issue['status'] } }).body;
        return { data: issue(0, { status: body.status }), error: undefined, response: ok };
      }
      throw new Error(`unexpected POST ${path}`);
    }),
    // Resolved, not bare: `patchIssue` destructures the answer, and a stub
    // returning `undefined` throws inside it — which the client reports as a
    // failed write, so every optimistic press would silently roll back.
    PATCH: vi.fn(async () => ({ data: undefined, error: undefined, response: ok })),
  };
}

const client = stubClient();

/// The stream is mocked so a test can hand the page a `project_changed`
/// frame; the real hook is a no-op here anyway (no token, no socket).
const stream = vi.hoisted(() => ({ onFrame: undefined as (() => void) | undefined }));
vi.mock('./useBoardStream', () => ({
  useBoardStream: (_projectId: string, _issue: number | null, onChange: () => void) => {
    stream.onFrame = onChange;
  },
}));

const auth = { logout: vi.fn(), baseUrl: 'http://column.test', token: null };
vi.mock('../../api/auth', () => ({
  useAdminClient: () => client,
  useAuth: () => auth,
}));

/// Stands in for `IssueDetailPage` and reports the origin it was handed, so
/// a test can assert the round trip rather than the shape of one link.
function DetailStub() {
  const from = (useLocation().state as { from?: unknown } | null)?.from;
  return <p>{typeof from === 'string' ? `came from ${from}` : 'issue detail'}</p>;
}

/// Whether the card's page was reached at all. Asserted through this rather
/// than through one of the stub's two strings: the stub prints the origin
/// when it is handed one, so a bare `queryByText('issue detail')` guard
/// stopped being able to fail the moment the origin started travelling.
function onDetailPage(): boolean {
  return screen.queryByText(/^(came from |issue detail$)/) !== null;
}

function renderColumn(status = 'backlog', query = '') {
  return render(
    <MemoryRouter initialEntries={[`/projects/${PROJECT.id}/board/${status}${query}`]}>
      <Routes>
        <Route path="/projects/:pid" element={<p>the whole board</p>} />
        <Route path="/projects/:pid/board/:status" element={<ColumnPage />} />
        <Route path="/projects/:pid/issues/:num" element={<DetailStub />} />
      </Routes>
    </MemoryRouter>,
  );
}

/// The card a title belongs to. Every press that is not "open the issue"
/// lives inside it.
function cardOf(node: HTMLElement): HTMLElement {
  const card = node.closest('article');
  if (card === null) throw new Error(`no card around ${node.textContent}`);
  return card;
}

/// The cards on screen, in the order they are drawn.
function renderedTitles(): (string | null)[] {
  return screen.getAllByTitle(/^issue |^Wire|^Blocked|^Cancelled/).map((n) => n.textContent);
}

function movesPosted(): unknown[][] {
  return client.POST.mock.calls.filter(
    ([path]) => path === '/v1/projects/{project_id}/issues/{number}/move',
  );
}

describe('ColumnPage', () => {
  beforeEach(() => {
    projectDto = PROJECT;
    failMoves = 0;
    stream.onFrame = undefined;
    client.POST.mockClear();
    client.PATCH.mockClear();
    client.GET.mockClear();
  });

  it('shows only the routed stage, one card per issue, unread lifted to the front', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    // The other columns' cards are on the tabs, not in the list.
    expect(screen.queryByText('Under way')).not.toBeInTheDocument();

    const rows = screen.getAllByTitle(/^issue |^Wire|^Blocked|^Cancelled/);
    // #2 carries something new, so it reads first — without `position`
    // being touched (the move that would write it is never sent here).
    expect(rows[0].textContent).toContain('Blocked one');
    expect(rows[1].textContent).toContain('Wire the board');
    expect(screen.getByText('⚑ Blocked')).toBeInTheDocument();
    const cancelled = screen.getByTitle('Cancelled one');
    expect(cancelled.className).toContain('line-through');
  });

  it('keeps every stage one press away, with the board’s own counts', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    const tabs = screen.getByRole('navigation', { name: 'Stages' });
    const active = within(tabs).getByText('BACKLOG').closest('a');
    expect(active?.getAttribute('aria-current')).toBe('page');
    // 2 live in backlog (the cancelled card is not live), 1 in progress.
    expect(within(tabs).getByText('IN PROG').closest('a')?.textContent).toContain('1');
    expect(active?.textContent).toContain('2');

    await userEvent.click(within(tabs).getByText('IN PROG'));
    expect(await screen.findByText('Under way')).toBeInTheDocument();
    expect(screen.queryByText('Wire the board')).not.toBeInTheDocument();
  });

  it('wears a dot for another stage holding something new, never for its own', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    // #4 in In Progress is unread; the dot is the only sign of it here. The
    // open stage's news is its rows' own pills, so its tab stays bare.
    expect(screen.getByTitle('Something new in In Progress')).toBeInTheDocument();
    expect(screen.queryByTitle('Something new in Backlog')).not.toBeInTheDocument();
  });

  it('bounces an unknown stage back to the board', async () => {
    renderColumn('shipping');
    expect(await screen.findByText('the whole board')).toBeInTheDocument();
  });

  it('narrows from the same URL vocabulary the board reads', async () => {
    renderColumn('backlog', '?q=blocked');
    await screen.findByText('Blocked one');

    expect(screen.queryByText('Wire the board')).not.toBeInTheDocument();
    // …and the tab admits the filter is holding cards back.
    const tabs = screen.getByRole('navigation', { name: 'Stages' });
    expect(within(tabs).getByText('BACKLOG').closest('a')?.textContent).toContain('1/2');
    expect(screen.getByLabelText('Filter the board (1 active)')).toBeInTheDocument();
  });

  it('opens the card from its row', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    await userEvent.click(screen.getByText('Wire the board'));
    // The stub reports the origin it was handed, so this also pins that a
    // card opened from here knows the way back.
    expect(
      await screen.findByText('came from /projects/01JPROJECT/board/backlog'),
    ).toBeInTheDocument();
  });

  it('opens the assignee’s profile from the row without opening the card', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    await userEvent.click(screen.getAllByTitle(/Open @dev-1's profile/)[0]);
    expect(
      await screen.findByRole('button', { name: /Close the agent profile/ }),
    ).toBeInTheDocument();
    expect(onDetailPage()).toBe(false);
  });

  it('moves a card to another column from its row, joining the end of the queue', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    await userEvent.click(screen.getByLabelText('Move #1 — currently in Backlog'));
    await userEvent.click(screen.getByRole('button', { name: 'In Progress' }));

    await waitFor(() => {
      expect(client.POST).toHaveBeenCalledWith(
        '/v1/projects/{project_id}/issues/{number}/move',
        expect.objectContaining({
          body: { status: 'in_progress', ordered_numbers: [4, 1] },
        }),
      );
    });
    // The row left this stage's list…
    expect(screen.queryByText('Wire the board')).not.toBeInTheDocument();
    // …and the drop that starts an agent is the one move that says so.
    expect(await screen.findByText('Queued for @dev-1 — #1')).toBeInTheDocument();
  });

  it('opens the status picker on Enter instead of lifting the row', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    // The row is the keyboard sensor's activator, so a keydown bubbling out
    // of the picker's trigger must press the trigger — not start a silent
    // keyboard drag whose next Enter posts a reorder nobody aimed.
    screen.getByLabelText('Move #1 — currently in Backlog').focus();
    await userEvent.keyboard('{Enter}');

    expect(await screen.findByRole('button', { name: 'In Progress' })).toBeInTheDocument();
  });

  it('refuses to start an unassigned card, out loud', async () => {
    renderColumn();
    await screen.findByText('Blocked one');
    const posts = client.POST.mock.calls.length;

    await userEvent.click(screen.getByLabelText('Move #2 — currently in Backlog'));
    await userEvent.click(screen.getByRole('button', { name: 'In Progress' }));

    // The whole sentence: the column named is the one the card snapped BACK
    // to, not the one that refused it.
    expect(
      await screen.findByText('Assign an agent first — #2 is back in Backlog'),
    ).toBeInTheDocument();
    // The card stayed put and nothing was sent.
    expect(screen.getByText('Blocked one')).toBeInTheDocument();
    expect(client.POST.mock.calls.length).toBe(posts);
  });

  it('rolls a failed move back, out loud, and Retry sends it again', async () => {
    failMoves = 1;
    renderColumn();
    await screen.findByText('Wire the board');

    await userEvent.click(screen.getByLabelText('Move #1 — currently in Backlog'));
    await userEvent.click(screen.getByRole('button', { name: 'In Progress' }));

    expect(
      await screen.findByText('Move failed, rolled back — the server said no'),
    ).toBeInTheDocument();
    // Rolled back: the row is still in this stage's list.
    expect(screen.getByText('Wire the board')).toBeInTheDocument();

    await userEvent.click(screen.getByRole('button', { name: 'Retry' }));
    await waitFor(() => {
      expect(screen.queryByText('Wire the board')).not.toBeInTheDocument();
    });
    expect(movesPosted()).toHaveLength(2);
  });

  it('opens the create modal on this column', async () => {
    renderColumn('review');
    await screen.findByTitle('New issue in Review');

    await userEvent.click(screen.getByTitle('New issue in Review'));
    await waitFor(() => {
      expect(screen.getByPlaceholderText('Issue title')).toBeInTheDocument();
    });
  });

  it('pins a card to the front of the stage, the same press the board takes', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    const rows = screen.getAllByTitle(/^issue |^Wire|^Blocked|^Cancelled/);
    await userEvent.click(
      within(cardOf(rows[1])).getByRole('button', {
        name: /Pin this card/,
      }),
    );

    expect(client.PATCH).toHaveBeenCalledWith('/v1/projects/{project_id}/issues/{number}', {
      params: { path: { project_id: PROJECT.id, number: 1 } },
      body: { pinned: true },
    });
    await waitFor(() => {
      const after = screen.getAllByTitle(/^issue |^Wire|^Blocked|^Cancelled/);
      expect(after[0].textContent).toContain('Wire the board');
    });
    // The row's own click opens the card; the pin must claim its press.
    expect(onDetailPage()).toBe(false);
  });

  it('reads an archived project without offering to work it', async () => {
    projectDto = { ...PROJECT, archived_at_ms: 111 };
    renderColumn();
    await screen.findByText('Wire the board');

    expect(screen.getByText('Archived — read only')).toBeInTheDocument();
    expect(screen.getByTitle('New issue in Backlog')).toBeDisabled();
    // Nothing on the row offers to move it either.
    expect(screen.queryByLabelText('Move #1 — currently in Backlog')).not.toBeInTheDocument();
  });

  it('names the stage it is showing, and how much work it holds', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    // The page's own heading, not a 10px label on a 210px queue.
    expect(screen.getByRole('heading', { level: 1 })).toHaveTextContent('Backlog');
    expect(screen.getByText('Kanban · Stage 1 of 5')).toBeInTheDocument();
    // Two live cards — the cancelled one is not work.
    expect(screen.getByTitle('2 live cards in Backlog')).toHaveTextContent('2');
    // …and the one thing waiting on the operator, named.
    expect(screen.getByTitle(/Cards with something new/)).toHaveTextContent('1 new');
    expect(screen.queryByText(/run failed/)).not.toBeInTheDocument();
  });

  it('counts the whole stage in its heading, not what the filter let through', async () => {
    renderColumn('backlog', '?q=blocked');
    await screen.findByText('Blocked one');

    // One row is on screen, but the stage still holds two live cards — a
    // heading that shrank with the filter would report an emptied stage.
    expect(screen.queryByText('Wire the board')).not.toBeInTheDocument();
    const block = screen.getByTitle('2 live cards in Backlog — 1 match the filter');
    expect(block).toHaveTextContent('2');
  });

  it('says nothing rather than printing zeroes on a quiet stage', async () => {
    renderColumn('todo');
    await screen.findByText('Nothing waiting on you');
    expect(screen.getByTitle('0 live cards in Todo')).toBeInTheDocument();
  });

  it('draws the reading order as bands, in the order the rows are drawn', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    // #2 is unread, #1 and #3 are not — so the lift the board applies
    // silently is on the page as two labelled runs of rows.
    const headers = screen.getAllByText(/^(Pinned|New|Queue)$/);
    expect(headers.map((node) => node.textContent)).toEqual(['New', 'Queue']);
    const rows = screen.getAllByTitle(/^issue |^Wire|^Blocked|^Cancelled/);
    expect(rows[0].textContent).toContain('Blocked one');
    expect(rows[1].textContent).toContain('Wire the board');
  });

  it('drops the band headers when there is only one band to name', async () => {
    renderColumn('in_progress');
    await screen.findByText('Under way');

    // Every card here is new, so a lone "New" header would separate nothing.
    expect(screen.queryByText('New')).not.toBeInTheDocument();
    expect(screen.queryByText('Queue')).not.toBeInTheDocument();
  });

  it('lifts a pinned card into its own band, above what is merely new', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    const rows = screen.getAllByTitle(/^issue |^Wire|^Blocked|^Cancelled/);
    await userEvent.click(
      within(cardOf(rows[1])).getByRole('button', {
        name: /Pin this card/,
      }),
    );

    await waitFor(() => {
      expect(screen.getAllByText(/^(Pinned|New|Queue)$/).map((n) => n.textContent)).toEqual([
        'Pinned',
        'New',
        'Queue',
      ]);
    });
    // What the operator chose leads what merely arrived.
    const after = screen.getAllByTitle(/^issue |^Wire|^Blocked|^Cancelled/);
    expect(after[0].textContent).toContain('Wire the board');
    expect(after[1].textContent).toContain('Blocked one');
  });

  it('lays the whole wall out as one grid, banded in the reading order', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    const headers = screen.getAllByText(/^(Pinned|New|Queue)$/);
    expect(headers.map((node) => node.textContent)).toEqual(['New', 'Queue']);
    expect(renderedTitles()).toEqual(['Blocked one', 'Wire the board', 'Cancelled one']);

    // **One** grid, not one per band. Three parents would mean React
    // unmounts a card that moves between bands — so pinning a card, or a
    // refetch that clears its unread, would destroy and rebuild the very
    // card being interacted with. The headers span the single grid.
    expect(document.querySelectorAll('[class*="grid-cols-1"]')).toHaveLength(1);
    for (const header of headers) {
      expect(header.closest('.col-span-full')).not.toBeNull();
    }
  });

  it('gives every card a keyboard door to its issue', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    // The card's own press is a mouse convenience. Dropping dnd-kit took
    // its `attributes` — and with them the tab stop that had been the only
    // non-mouse way to open an issue from this page.
    const link = screen.getByRole('link', { name: 'Wire the board' });
    expect(link).toHaveAttribute('href', `/projects/${PROJECT.id}/issues/1`);

    link.focus();
    expect(document.activeElement).toBe(link);
    await userEvent.keyboard('{Enter}');
    expect(
      await screen.findByText('came from /projects/01JPROJECT/board/backlog'),
    ).toBeInTheDocument();
  });

  it('does not transform a card on hover, which would trap the Move panel', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    // A non-`none` `translate` makes the card a stacking context, confining
    // the picker's `z-30` panel to it while every later card paints on top.
    // Measured at 13% of the panel left hit-testable — and a press whose
    // down and up land on different elements fires `click` on their common
    // ancestor, so the option does nothing at all.
    for (const card of screen.getAllByRole('article')) {
      expect(card.className).not.toMatch(/translate/);
      expect(card.className).not.toMatch(/hover:scale/);
    }
  });

  it('offers no way to reorder a stage from here', async () => {
    renderColumn();
    await screen.findByText('Wire the board');

    // Deliberate: `position` is a one-dimensional rank and a grid cannot
    // show one without lying about which of two side-by-side cards leads.
    // The board is where an order is dragged into shape; this page reads,
    // triages and hands on. Nothing here may claim otherwise.
    for (const card of screen.getAllByRole('article')) {
      expect(card).not.toHaveAttribute('draggable', 'true');
      expect(card.className).not.toContain('touch-none');
    }
    expect(document.querySelector('[role="button"][aria-roledescription]')).toBeNull();
  });

  it('gives a long title two lines before it gives up', async () => {
    renderColumn();
    const title = await screen.findByTitle('Wire the board');

    // The one cost the measured grid charged was truncated titles. A card
    // has the vertical room a row never did, so it spends it here.
    expect(title.className).toContain('line-clamp-2');
  });

  it('sends the card off knowing where to come back to', async () => {
    renderColumn('backlog', '?q=blocked');
    await screen.findByText('Blocked one');

    // The detail page's back link is fixed on the board, which from a
    // maximized stage is two steps from where the operator was — and drops
    // this stage's filter on the way, so the wall they came back to was not
    // the wall they left. Both doors out of a card carry the origin: the
    // title's link and the card's own press.
    const link = screen.getByRole('link', { name: 'Blocked one' });
    await userEvent.click(link);
    expect(await screen.findByText('came from /projects/01JPROJECT/board/backlog?q=blocked'))
      .toBeInTheDocument();
  });

  it('carries the origin on the card’s own press too, not just the title link', async () => {
    renderColumn('backlog', '?q=blocked');
    await screen.findByText('Blocked one');

    // The whole card is a mouse target; only the title is a link. If just
    // one of them recorded the origin, which door you used would decide
    // where Back went.
    await userEvent.click(cardOf(screen.getByTitle('Blocked one')));
    expect(await screen.findByText('came from /projects/01JPROJECT/board/backlog?q=blocked'))
      .toBeInTheDocument();
  });

  it('walks back to the board without dropping the narrowing', async () => {
    renderColumn('backlog', '?q=blocked');
    await screen.findByText('Blocked one');

    const back = screen.getByText('Board').closest('a');
    expect(back?.getAttribute('href')).toBe(`/projects/${PROJECT.id}?q=blocked`);
  });

});
