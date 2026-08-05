import { describe, expect, it, vi, beforeEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { ActivityDrawer } from './ActivityDrawer';
import type { IssueEvent } from './timelineModel';

// The wiring the model tests can't reach: that a comment is shown rather
// than narrated (the same rule the per-issue timeline follows), and that
// an entry opens the card it belongs to.

const feed = vi.hoisted(() => ({ fetchFeed: vi.fn() }));
vi.mock('./api', () => ({ fetchFeed: feed.fetchFeed }));
vi.mock('../../api/auth', () => ({
  useAdminClient: () => ({}),
  useAuth: () => ({ logout: vi.fn() }),
}));

function entry(id: string, number: number, body: IssueEvent['body']): IssueEvent {
  return { id, number, actor: 'dev-1', actor_is_agent: true, body, created_at_ms: 0 };
}

describe('ActivityDrawer', () => {
  beforeEach(() => {
    feed.fetchFeed.mockReset();
  });

  it('shows comments verbatim and narrates system entries', async () => {
    feed.fetchFeed.mockResolvedValue({
      kind: 'ok',
      value: [
        entry('a', 4, { kind: 'comment', text: 'reproduced it' }),
        entry('b', 4, { kind: 'moved', from: 'todo', to: 'in_progress' }),
      ],
    });
    render(
      <ActivityDrawer projectId="p" refreshKey={0} onClose={vi.fn()} onOpenIssue={vi.fn()} />,
    );
    expect(await screen.findByText('reproduced it')).toBeInTheDocument();
    expect(screen.getByText(/moved it from/)).toBeInTheDocument();
  });

  it('opens the card an entry belongs to', async () => {
    const onOpenIssue = vi.fn();
    feed.fetchFeed.mockResolvedValue({
      kind: 'ok',
      value: [entry('a', 7, { kind: 'opened' })],
    });
    render(
      <ActivityDrawer projectId="p" refreshKey={0} onClose={vi.fn()} onOpenIssue={onOpenIssue} />,
    );
    await userEvent.click(await screen.findByText(/opened this issue/));
    expect(onOpenIssue).toHaveBeenCalledWith(7);
  });

  it('says so when the board has no history yet', async () => {
    feed.fetchFeed.mockResolvedValue({ kind: 'ok', value: [] });
    render(
      <ActivityDrawer projectId="p" refreshKey={0} onClose={vi.fn()} onOpenIssue={vi.fn()} />,
    );
    expect(await screen.findByText(/Nothing has happened/)).toBeInTheDocument();
  });

  it('shows the reason when the feed cannot be read', async () => {
    feed.fetchFeed.mockResolvedValue({ kind: 'failed', message: 'HTTP Error 500' });
    render(
      <ActivityDrawer projectId="p" refreshKey={0} onClose={vi.fn()} onOpenIssue={vi.fn()} />,
    );
    expect(await screen.findByText('HTTP Error 500')).toBeInTheDocument();
  });
});
