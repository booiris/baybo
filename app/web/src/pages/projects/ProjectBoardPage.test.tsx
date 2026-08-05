import { describe, expect, it, vi } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, Route, Routes } from 'react-router-dom';

import { ProjectBoardPage } from './ProjectBoardPage';
import type { Issue, Project } from './boardModel';

// The board renders five fixed columns from a flat listing, counts only
// live work in each header, and marks the flags a card can carry. This is
// the wiring a reducer test can't reach: that the page asks for the right
// things and paints what comes back.

const PROJECT: Project = {
  id: '01JPROJECT',
  name: 'Kanban',
  description: '',
  workdir: '/tmp/kanban',
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
    created_at_ms: 0,
    updated_at_ms: 0,
    ...overrides,
  };
}

const ISSUES: Issue[] = [
  issue(1, { title: 'Wire the board', position: 0 }),
  issue(2, { title: 'Blocked one', position: 1, blocked_reason: 'waiting on tmux' }),
  issue(3, { title: 'Cancelled one', position: 2, cancelled_at_ms: 111 }),
  issue(4, { title: 'Under way', status: 'in_progress', position: 0, priority: 'urgent' }),
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
      throw new Error(`unexpected GET ${path}`);
    }),
    POST: vi.fn(),
    PATCH: vi.fn(),
  };
}

const client = stubClient();

vi.mock('../../api/auth', () => ({
  useAdminClient: () => client,
  useAuth: () => ({ logout: vi.fn() }),
}));

function renderBoard() {
  return render(
    <MemoryRouter initialEntries={[`/projects/${PROJECT.id}`]}>
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
    // Three empty columns say so rather than rendering as blank space.
    expect(screen.getAllByText('No issues')).toHaveLength(3);
  });

  it('counts live work only, so a cancelled card does not inflate a column', async () => {
    renderBoard();
    await screen.findByText('Wire the board');

    // Backlog holds #1, #2 and cancelled #3 — but #3 is hidden by default
    // and would not count even when shown.
    expect(screen.queryByText('Cancelled one')).not.toBeInTheDocument();
    const backlog = screen.getByRole('heading', { name: 'Backlog' }).parentElement;
    expect(backlog?.textContent).toContain('2');
  });

  it('shows a cancelled card struck through once the filter is on', async () => {
    renderBoard();
    await screen.findByText('Wire the board');

    await userEvent.click(screen.getByLabelText('Show cancelled'));
    const cancelled = await screen.findByText('Cancelled one');
    expect(cancelled.className).toContain('line-through');
    // …and it still is not counted as outstanding work.
    const backlog = screen.getByRole('heading', { name: 'Backlog' }).parentElement;
    expect(backlog?.textContent).toContain('2');
  });

  it('marks a blocked card and a priority without a column of their own', async () => {
    renderBoard();
    await screen.findByText('Blocked one');

    expect(screen.getByText('⚑ Blocked')).toBeInTheDocument();
    // Urgent renders its mark on the card face; priority never reorders.
    expect(screen.getByText('▲▲')).toBeInTheDocument();
  });

  it('opens the create modal on the column it was pressed in', async () => {
    renderBoard();
    await screen.findByText('Wire the board');

    await userEvent.click(screen.getByTitle('New issue in Review'));
    await waitFor(() => {
      expect(screen.getByPlaceholderText('Issue title')).toBeInTheDocument();
    });
    // The pre-filled column is the whole reason the modal knows the status.
    const modal = screen.getByPlaceholderText('Issue title').closest('div')?.parentElement;
    expect(modal?.textContent).toContain('Review');
  });
});
