import { describe, expect, it, vi } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
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
    ...overrides,
  };
}

const ISSUES: Issue[] = [
  issue(1, { title: 'Wire the board', position: 0, assignee: 'dev-1' }),
  issue(2, { title: 'Blocked one', position: 1, blocked_reason: 'waiting on tmux' }),
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
        return { data: { items: ISSUES }, error: undefined, response: ok };
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
    POST: vi.fn(),
    PATCH: vi.fn(),
  };
}

const client = stubClient();

const auth = { logout: vi.fn() };
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

    await userEvent.type(screen.getByLabelText(/Filter cards by title/), 'blocked');
    await screen.findByText('Blocked one');
    expect(screen.queryByText('Wire the board')).not.toBeInTheDocument();
    expect(client.GET.mock.calls.length).toBe(before);
    expect(rerender).toBeTypeOf('function');
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

  it('floats a side panel over the board instead of docking it', async () => {
    renderBoard();
    await screen.findByText('Wire the board');
    await userEvent.click(screen.getByRole('button', { name: 'Activity' }));

    const panel = await screen.findByRole('complementary');
    expect(panel.parentElement?.className).toContain('absolute');
    expect(screen.getByText('Wire the board')).toBeInTheDocument();
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