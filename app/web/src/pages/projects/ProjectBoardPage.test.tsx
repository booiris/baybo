import { beforeEach, describe, expect, it, vi } from 'vitest';
import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, Route, Routes } from 'react-router-dom';

import { ProjectBoardPage } from './ProjectBoardPage';
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
    opened_by_agent: false,
    pinned: false,
    ...overrides,
  };
}

const ISSUES: Issue[] = [
  issue(1, { title: 'Wire the board', position: 0, assignee: 'dev-1', opened_by_agent: true }),
  issue(2, { title: 'Blocked one', position: 1, blocked_reason: 'waiting on tmux', unread: 2 }),
  issue(3, { title: 'Cancelled one', position: 2, cancelled_at_ms: 111 }),
  issue(4, {
    title: 'Under way',
    status: 'in_progress',
    position: 0,
    stage: 0,
    priority: 'urgent',
    assignee: 'dev-1',
  }),
];

const RUNS = [
  { number: 4, attempt: 1, agent_id: 'dev-1', status: 'running', trigger: 'started', created_at_ms: 0 },
  { number: 1, attempt: 1, agent_id: 'dev-1', status: 'queued', trigger: 'started', created_at_ms: 0 },
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
const noContent = { status: 204, ok: true } as Response;

let boardIsRead = false;

function stubClient() {
  return {
    GET: vi.fn(async (path: string) => {
      if (path === '/v1/projects') {
        return { data: { items: [PROJECT] }, error: undefined, response: ok };
      }
      if (path === '/v1/projects/{project_id}') {
        return { data: PROJECT, error: undefined, response: ok };
      }
      if (path === '/v1/projects/{project_id}/issues') {
        // The board-wide read is the one action here whose whole point is
        // that the board comes back different, so the refetch has to be
        // able to answer with cleared cards rather than the same ones.
        const items = boardIsRead ? ISSUES.map((row) => ({ ...row, unread: 0 })) : ISSUES;
        return { data: { items }, error: undefined, response: ok };
      }
      if (path === '/v1/projects/{project_id}/runs') {
        return { data: { items: RUNS }, error: undefined, response: ok };
      }
      if (path === '/v1/projects/{project_id}/agents') {
        return { data: { items: TEAM }, error: undefined, response: ok };
      }
      if (path === '/v1/projects/{project_id}/feed') {
        return { data: { items: [] }, error: undefined, response: ok };
      }
      if (path === '/v1/projects/attention') {
        return { data: { items: [] }, error: undefined, response: ok };
      }
      throw new Error(`unexpected GET ${path}`);
    }),
    POST: vi.fn(async (path: string) => {
      if (path === '/v1/projects/{project_id}/read') {
        boardIsRead = true;
        return { data: undefined, error: undefined, response: noContent };
      }
      throw new Error(`unexpected POST ${path}`);
    }),
    // Resolved, not bare: `patchIssue` destructures the answer, and a stub
    // returning `undefined` throws inside it — which the client reports as a
    // failed write, so every optimistic press would silently roll back. The
    // return type is written out so a test can hand it a refusal instead.
    PATCH: vi.fn(
      async (): Promise<{
        data: undefined;
        error: { error: string } | undefined;
        response: Response;
      }> => ({ data: undefined, error: undefined, response: ok }),
    ),
  };
}

const client = stubClient();

// No token on purpose: `useBoardStream` opens a real WebSocket the moment it
// has one, and a socket that cannot connect reconnects — each reconnect
// bumping `refreshKey` and refetching the whole board underneath the
// assertions. `baseUrl` is here because the portraits hook reads it.
const auth = { logout: vi.fn(), baseUrl: 'http://board.test', token: null };
vi.mock('../../api/auth', () => ({
  useAdminClient: () => client,
  useAuth: () => auth,
}));

function renderBoard(query = '') {
  return render(
    <MemoryRouter initialEntries={[`/projects/${PROJECT.id}${query}`]}>
      <Routes>
        <Route path="/projects/:pid" element={<ProjectBoardPage />} />
      </Routes>
    </MemoryRouter>,
  );
}

describe('ProjectBoardPage', () => {
  beforeEach(() => {
    boardIsRead = false;
  });

  it('paints five columns and files each card under its own', async () => {
    renderBoard();

    await screen.findByText('Wire the board');
    for (const label of ['Backlog', 'Todo', 'In Progress', 'Review', 'Done']) {
      expect(screen.getByRole('heading', { name: label })).toBeInTheDocument();
    }
    expect(screen.getByText('Under way')).toBeInTheDocument();
    // The empty columns say what to do about it, not just that they are
    // empty — the placeholder is the only affordance a new board shows.
    expect(screen.getAllByText(/No issues/)).toHaveLength(3);
    expect(screen.getAllByText(/Drag one in/)).toHaveLength(3);
  });

  it('shows a cancelled card struck through, without counting it as live work', async () => {
    renderBoard();
    await screen.findByText('Wire the board');

    // Visible by default: cancel is the terminal negative, not a delete.
    const cancelled = await screen.findByText('Cancelled one');
    expect(cancelled.className).toContain('line-through');
    // …but the column count measures live work, so it is not in the number.
    const backlog = screen.getByRole('heading', { name: 'Backlog' }).parentElement;
    expect(backlog?.textContent).toContain('2');
  });

  it('takes cancelled cards away when the filter asks it to', async () => {
    renderBoard();
    await screen.findByText('Cancelled one');

    await userEvent.click(screen.getByLabelText('Filter the board'));
    await userEvent.click(screen.getByLabelText('Hide cancelled'));
    expect(screen.queryByText('Cancelled one')).not.toBeInTheDocument();
    const backlog = screen.getByRole('heading', { name: 'Backlog' }).parentElement;
    expect(backlog?.textContent).toContain('2');
  });

  it('marks a blocked card and a priority without a column of their own', async () => {
    renderBoard();
    await screen.findByText('Blocked one');

    expect(screen.getByText('⚑ Blocked')).toBeInTheDocument();
    expect(screen.getByText('▲▲')).toBeInTheDocument();
  });

  it('says which cards are working and which are waiting', async () => {
    renderBoard();
    await screen.findByText('Under way');

    expect(screen.getByText('working')).toBeInTheDocument();
    expect(screen.getByText('queued')).toBeInTheDocument();
    expect(screen.getAllByText(/^(working|queued)$/)).toHaveLength(2);
  });

  it('opens the create modal on the column it was pressed in', async () => {
    renderBoard();
    await screen.findByText('Wire the board');

    await userEvent.click(screen.getByTitle('New issue in Review'));
    await waitFor(() => {
      expect(screen.getByPlaceholderText('Issue title')).toBeInTheDocument();
    });
    const modal = screen.getByPlaceholderText('Issue title').closest('div')?.parentElement;
    expect(modal?.textContent).toContain('Review');
  });

  it('does not refetch when the filter changes', async () => {
    const { rerender } = renderBoard();
    await screen.findByText('Wire the board');
    const before = client.GET.mock.calls.length;

    await userEvent.click(screen.getByLabelText('Filter the board'));
    await userEvent.type(screen.getByLabelText(/Filter cards by title/), 'blocked');
    await screen.findByText('Blocked one');
    expect(screen.queryByText('Wire the board')).not.toBeInTheDocument();
    expect(client.GET.mock.calls.length).toBe(before);
    expect(rerender).toBeTypeOf('function');
  });

  it('says from the header how many ways the board is being narrowed', async () => {
    renderBoard('?q=blocked&blocked=1');
    await screen.findByText('Blocked one');

    // Collapsing the strip into a button is only safe if a board that is
    // holding cards back cannot look like one that is not.
    const trigger = screen.getByLabelText('Filter the board (2 active)');
    expect(trigger.className).toContain('bg-brand');
    expect(trigger.textContent).toContain('2');
  });

  it('narrows the board from the URL without refetching it', async () => {
    renderBoard('?q=blocked');
    await screen.findByText('Blocked one');
    expect(screen.queryByText('Wire the board')).not.toBeInTheDocument();

    expect(await screen.findByTitle(/1 of 2 live cards match/)).toBeInTheDocument();
  });

  it('shows cancelled cards only when the URL asks for them', async () => {
    renderBoard('?cancelled=1');
    expect(await screen.findByText('Cancelled one')).toBeInTheDocument();
    expect(await screen.findByTitle('2 live')).toBeInTheDocument();
  });

  it('slides the panel in, and back out on a press anywhere else', async () => {
    renderBoard();
    await screen.findByText('Wire the board');
    await userEvent.click(screen.getByRole('button', { name: 'Activity' }));

    const panel = (await screen.findByRole('complementary')).parentElement;
    expect(panel?.className).toContain('transition-transform');
    await waitFor(() => {
      expect(panel?.className).toContain('translate-x-0');
    });

    // A press outside is one of the three ways it leaves, and it leaves the
    // way it arrived — the parent's unmount waits for the slide.
    await userEvent.click(screen.getByRole('heading', { name: 'Backlog' }));
    expect(panel?.className).toContain('translate-x-full');
    await waitFor(() => {
      expect(screen.queryByRole('complementary')).toBeNull();
    });
  });

  it('closes the panel on Escape', async () => {
    renderBoard();
    await screen.findByText('Wire the board');
    await userEvent.click(screen.getByRole('button', { name: 'Activity' }));
    await screen.findByRole('complementary');

    await userEvent.keyboard('{Escape}');
    await waitFor(() => {
      expect(screen.queryByRole('complementary')).toBeNull();
    });
  });

  it('floats a side panel over the board instead of docking it', async () => {
    renderBoard();
    await screen.findByText('Wire the board');
    await userEvent.click(screen.getByRole('button', { name: 'Activity' }));

    const panel = await screen.findByRole('complementary');
    expect(panel.parentElement?.className).toContain('absolute');
    expect(screen.getByText('Wire the board')).toBeInTheDocument();
  });

  it('lifts a card with something new to the top of its column', async () => {
    renderBoard();
    await screen.findByText('Wire the board');

    // #2 sits under #1 by position, and what an agent said moves it up the
    // column the operator is reading — without touching `position`, which
    // is what a move writes.
    const backlog = screen.getByRole('heading', { name: 'Backlog' }).closest('section');
    const cards = within(backlog as HTMLElement).getAllByRole('article');
    expect(cards[0].textContent).toContain('Blocked one');
    expect(cards[1].textContent).toContain('Wire the board');
  });

  it('narrows to what is new from the filter menu', async () => {
    renderBoard();
    await screen.findByText('Wire the board');

    await userEvent.click(screen.getByLabelText('Filter the board'));
    await userEvent.click(screen.getByLabelText('Unread only'));
    expect(screen.queryByText('Wire the board')).not.toBeInTheDocument();
    expect(screen.getByText('Blocked one')).toBeInTheDocument();
  });

  it('shows only the cards with something new when asked', async () => {
    renderBoard('?unread=1');
    await screen.findByText('Blocked one');

    expect(screen.queryByText('Wire the board')).not.toBeInTheDocument();
    expect(screen.queryByText('Under way')).not.toBeInTheDocument();
    // …and the header still admits the board is holding cards back.
    expect(screen.getByLabelText('Filter the board (1 active)')).toBeInTheDocument();
  });

  it('reads the whole board in one press, and then has nothing left to do', async () => {
    renderBoard();
    await screen.findByText('Wire the board');

    await userEvent.click(screen.getByRole('button', { name: 'Mark read 2' }));
    expect(client.POST).toHaveBeenCalledWith('/v1/projects/{project_id}/read', {
      params: { path: { project_id: PROJECT.id } },
    });

    // The button stays where it is rather than disappearing under the press
    // that emptied it — the group behind it must not shuffle.
    await waitFor(() => {
      expect(screen.getByRole('button', { name: 'Mark read' })).toBeDisabled();
    });
    expect(screen.queryByTitle(/new since you opened this card/)).toBeNull();
  });

  it('reads a pinned card above the one carrying something new', async () => {
    // The rank, on one board: #2 is unread and leads the column to start
    // with. Pinning #1 — which has nothing new on it at all — puts it in
    // front, because what the operator chose outranks what arrived.
    renderBoard();
    await screen.findByText('Wire the board');

    const backlog = screen.getByRole('heading', { name: 'Backlog' }).closest('section');
    const before = within(backlog as HTMLElement).getAllByRole('article');
    expect(before[0].textContent).toContain('Blocked one');

    await userEvent.click(within(before[1]).getByRole('button', { name: /Pin this card/ }));

    expect(client.PATCH).toHaveBeenCalledWith('/v1/projects/{project_id}/issues/{number}', {
      params: { path: { project_id: PROJECT.id, number: 1 } },
      body: { pinned: true },
    });
    await waitFor(() => {
      const cards = within(backlog as HTMLElement).getAllByRole('article');
      expect(cards[0].textContent).toContain('Wire the board');
      expect(cards[1].textContent).toContain('Blocked one');
    });
    // The press must not also open the card — the card's own click
    // navigates, so the pin has to claim the event.
    expect(screen.getByRole('heading', { name: 'Backlog' })).toBeInTheDocument();
  });

  it('marks the cards an agent filed, at the head of the meta row', async () => {
    // Only these cards are the board's to groom out of Backlog; the
    // operator's stay where they were put. Unmarked has to mean the second
    // one, so the mark sits with the number that identifies the card.
    renderBoard();
    const ours = (await screen.findByText('Wire the board')).closest('article') as HTMLElement;
    const mark = within(ours).getByTitle(/^Filed by an agent/);
    const number = within(ours).getByText('#1');
    expect(number.compareDocumentPosition(mark) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();

    const theirs = screen.getByText('Blocked one').closest('article') as HTMLElement;
    expect(within(theirs).queryByTitle(/^Filed by an agent/)).toBeNull();
  });

  it("wears the pin at the meta row's right end, in front of the time", async () => {
    renderBoard();
    const card = (await screen.findByText('Wire the board')).closest('article') as HTMLElement;

    // The head of the meta row is where the card is identified — its mark
    // and its number — and the corner past the time is the unread count's.
    const number = within(card).getByText('#1');
    const pin = within(card).getByRole('button', { name: /Pin this card/ });
    const time = within(card).getByTitle(/^Last touched/);
    expect(number.compareDocumentPosition(pin) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(pin.compareDocumentPosition(time) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  it('puts the card back where it was when the pin fails to land', async () => {
    client.PATCH.mockResolvedValueOnce({
      data: undefined,
      error: { error: 'nope' },
      response: { status: 500, ok: false } as Response,
    });
    renderBoard();
    await screen.findByText('Wire the board');

    const backlog = screen.getByRole('heading', { name: 'Backlog' }).closest('section');
    await userEvent.click(
      within(within(backlog as HTMLElement).getAllByRole('article')[1]).getByRole('button', {
        name: /Pin this card/,
      }),
    );

    // The pin is the one mark on this board whose truth is the server's, so
    // a card left floating on a write that never landed would be the board
    // lying about the operator's own instruction.
    expect(await screen.findByText(/Pin failed/)).toBeInTheDocument();
    const cards = within(backlog as HTMLElement).getAllByRole('article');
    expect(cards[0].textContent).toContain('Blocked one');
    expect(cards[1].textContent).toContain('Wire the board');
  });

  it('opens the assignee’s profile from the card without opening the card', async () => {
    renderBoard();
    await screen.findByText('Under way');

    // The card's own click navigates. Without stopping the event the
    // avatar could never be the profile's entry point the mockup says it is.
    await userEvent.click(screen.getAllByTitle(/Open @dev-1's profile/)[0]);
    expect(await screen.findByRole('button', { name: /Close the agent profile/ })).toBeInTheDocument();
  });
});