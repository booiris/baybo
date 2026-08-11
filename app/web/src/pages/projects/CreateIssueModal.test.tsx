import { describe, expect, it, vi, beforeEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { CreateIssueModal } from './CreateIssueModal';
import type { Agent } from './boardModel';

const api = vi.hoisted(() => ({ createIssue: vi.fn() }));
vi.mock('./api', () => api);
vi.mock('../../api/auth', () => ({
  useAdminClient: () => ({}),
  useAuth: () => ({ logout: vi.fn() }),
}));

const DEV: Agent = {
  id: '01JDEV',
  handle: 'dev-1',
  name: 'Dev One',
  description: 'writes things',
  framework: 'baybo',
  lead: false,
  created_at_ms: 0,
};

function open(status: Parameters<typeof CreateIssueModal>[0]['status'] = 'backlog') {
  const onCreated = vi.fn();
  render(
    <CreateIssueModal
      projectId="01JP"
      status={status}
      agents={[DEV]}
      portrait={(id) => (id == null ? null : `data:face/${id}`)}
      parents={[{ number: 5, title: 'Auth revamp' }]}
      onClose={vi.fn()}
      onCreated={onCreated}
    />,
  );
  return { onCreated };
}

describe('CreateIssueModal', () => {
  beforeEach(() => {
    api.createIssue.mockReset().mockResolvedValue({ kind: 'ok', value: { number: 9 } });
  });

  it('opens on the column it was pressed in, but does not lock it there', async () => {
    open('todo');
    // Chips that say what they are set to and open a panel of the board's own
    // pills — not native selects drawing an OS menu inside a hand-drawn modal.
    await userEvent.click(screen.getByLabelText('Status: Todo'));
    await userEvent.click(screen.getByRole('button', { name: 'Review' }));
    expect(screen.getByLabelText('Status: Review')).toBeInTheDocument();
  });

  it('carries the board’s own vocabulary into every chip', async () => {
    open('todo');
    // The same three properties are set on the issue page's rail, from the
    // same lists — priority in its tone, the assignee by face.
    expect(screen.getByLabelText('Priority: No priority')).toBeInTheDocument();
    await userEvent.click(screen.getByLabelText('Assignee: Unassigned'));
    const option = screen.getByRole('button', { name: '@dev-1' });
    expect(option.querySelector('img')).toHaveAttribute('src', 'data:face/01JDEV');
  });

  it('warns before creating work that starts an agent', async () => {
    open('in_progress');
    // Without somebody on it, In Progress is refused rather than announced.
    expect(screen.getByText(/needs an assignee/)).toBeInTheDocument();

    await userEvent.click(screen.getByLabelText('Assignee: Unassigned'));
    await userEvent.click(screen.getByRole('button', { name: '@dev-1' }));
    // With somebody on it, creating spends money — say so before the click.
    expect(screen.getByText(/starts a run/)).toBeInTheDocument();
    expect(screen.queryByText(/needs an assignee/)).not.toBeInTheDocument();
  });

  it('files a card as a step of another', async () => {
    open();
    await userEvent.type(screen.getByPlaceholderText('Issue title'), 'Token refresh');
    await userEvent.click(screen.getByLabelText('Parent: No parent'));
    await userEvent.click(screen.getByRole('button', { name: '#5 Auth revamp' }));
    await userEvent.click(screen.getByRole('button', { name: /Create issue/ }));

    expect(api.createIssue).toHaveBeenCalledWith(
      {},
      '01JP',
      expect.objectContaining({ parent: 5, stage: 0 }),
    );
  });

  it('sends nothing about a parent when there is none', async () => {
    open();
    await userEvent.type(screen.getByPlaceholderText('Issue title'), 'Standalone');
    await userEvent.click(screen.getByRole('button', { name: /Create issue/ }));

    const body = api.createIssue.mock.calls[0][2] as Record<string, unknown>;
    expect(body).not.toHaveProperty('parent');
    expect(body).not.toHaveProperty('stage');
  });
});
