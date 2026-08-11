import { describe, expect, it, vi, beforeEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { ActivityDrawer } from './ActivityDrawer';
import type { IssueEvent } from './timelineModel';


const feed = vi.hoisted(() => ({ fetchFeed: vi.fn() }));
vi.mock('./api', () => ({ fetchFeed: feed.fetchFeed }));
vi.mock('../../api/auth', () => ({
  useAdminClient: () => ({}),
  useAuth: () => ({ logout: vi.fn() }),
}));

function entry(id: string, number: number, body: IssueEvent['body']): IssueEvent {
  return {
    id,
    number,
    actor: { kind: 'agent', id: '01JC3KQ4Z8AAAAAAAAAAAAAAAA', handle: 'dev-1' },
    body,
    created_at_ms: 0,
  };
}

describe('ActivityDrawer', () => {
  beforeEach(() => {
    feed.fetchFeed.mockReset();
  });

  it('narrates entries without pouring a comment into the feed', async () => {
    // A real agent's run report runs to hundreds of words. The feed is a
    // list of things that happened, so it says *that* one was left and
    // links to the card; the words live on the card's own timeline.
    const report = 'Root cause confirmed. '.repeat(40);
    feed.fetchFeed.mockResolvedValue({
      kind: 'ok',
      value: [
        entry('a', 4, { kind: 'comment', text: report }),
        entry('b', 4, { kind: 'moved', from: 'todo', to: 'in_progress' }),
      ],
    });
    render(
      <ActivityDrawer projectId="p" refreshKey={0} onClose={vi.fn()} onOpenIssue={vi.fn()} />,
    );
    // Every line names its card: `describeEvent`'s "moved it to Review" is
    // written for a pane that is one card, and names nothing in a feed.
    expect(
      await screen.findByRole('button', { name: /@dev-1 commented on #4/ }),
    ).toBeInTheDocument();
    expect(screen.queryByText(/Root cause confirmed/)).not.toBeInTheDocument();
    expect(
      screen.getByRole('button', { name: /@dev-1 moved #4 Todo → In Progress/ }),
    ).toBeInTheDocument();
  });

  it('colours each line by what happened, so a failure is not a hire', async () => {
    feed.fetchFeed.mockResolvedValue({
      kind: 'ok',
      value: [
        entry('a', 9, { kind: 'run_settled', attempt: 3, status: 'failed', error: 'boom' }),
        entry('b', 7, { kind: 'run_settled', attempt: 1, status: 'done' }),
        entry('c', 8, { kind: 'blocked', reason: 'sandbox has no tmux' }),
      ],
    });
    render(
      <ActivityDrawer projectId="p" refreshKey={0} onClose={vi.fn()} onOpenIssue={vi.fn()} />,
    );
    await screen.findByRole('button', { name: /run #3 failed on #9 — boom/ });
    // The whole point of the feed is being skimmed down its left edge before
    // any of it is read.
    const dots = screen.getByRole('complementary').querySelectorAll('span[aria-hidden]');
    expect([...dots].map((dot) => dot.className)).toEqual([
      expect.stringContaining('bg-err'),
      expect.stringContaining('bg-ok'),
      expect.stringContaining('bg-warn'),
    ]);
  });

  it('shows a hire, which belongs to the board and opens no card', async () => {
    const onOpenIssue = vi.fn();
    feed.fetchFeed.mockResolvedValue({
      kind: 'ok',
      value: [
        {
          actor: { kind: 'agent', id: '01JC3KQ4Z8AAAAAAAAAAAAAAAA', handle: 'lead' },
          body: {
            kind: 'hired',
            agent: { id: '01JC3KQ4Z8BBBBBBBBBBBBBBBB', handle: 'tester' },
          },
          created_at_ms: 0,
        },
      ],
    });
    render(
      <ActivityDrawer projectId="p" refreshKey={0} onClose={vi.fn()} onOpenIssue={onOpenIssue} />,
    );
    const named = await screen.findByText('@tester');
    expect(screen.getByRole('complementary').textContent).toContain('@lead hired @tester');
    // No card to open, so it is not a button that goes nowhere.
    expect(screen.queryByRole('button', { name: /hired/ })).toBeNull();
    await userEvent.click(named);
    expect(onOpenIssue).not.toHaveBeenCalled();
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
    await userEvent.click(await screen.findByRole('button', { name: /@dev-1 opened #7/ }));
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
