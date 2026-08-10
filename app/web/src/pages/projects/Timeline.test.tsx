import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { Timeline } from './Timeline';
import type { Issue } from './boardModel';
import type { IssueEvent, IssueEventBody } from './timelineModel';


const ISSUE: Issue = {
  number: 4,
  project_id: '01JPROJECT',
  title: 'Wire the board',
  description: '',
  status: 'in_progress',
  priority: 'none',
  assignee: 'dev-1',
  position: 0,
  stage: 0,
  created_at_ms: 0,
  updated_at_ms: 0,
};

let seq = 0;
function entry(body: IssueEventBody, handle?: string): IssueEvent {
  seq += 1;
  return {
    id: `evt-${seq}`,
    number: 4,
    actor:
      handle == null
        ? { kind: 'user' }
        : { kind: 'agent', id: '01JC3KQ4Z8AAAAAAAAAAAAAAAA', handle },
    body,
    created_at_ms: Date.now(),
  };
}

function renderTimeline(events: IssueEvent[], onComment = vi.fn(), busy = false) {
  render(
    <Timeline
      events={events}
      issue={ISSUE}
      runs={[]}
      onComment={onComment}
      onResolveApproval={vi.fn()}
      busy={busy}
    />,
  );
  return onComment;
}

describe('Timeline', () => {
  it('narrates system entries and shows comments as their own text', () => {
    renderTimeline([
      entry({ kind: 'opened' }),
      entry({ kind: 'moved', from: 'todo', to: 'in_progress' }),
      entry({ kind: 'comment', text: 'check the reconnect path' }, 'dev-1'),
      entry({ kind: 'run_settled', attempt: 2, status: 'failed', error: 'ran out' }, 'dev-1'),
    ]);

    expect(screen.getByText('opened this issue')).toBeInTheDocument();
    expect(screen.getByText('moved it from Todo to In Progress')).toBeInTheDocument();
    expect(screen.getByText('check the reconnect path')).toBeInTheDocument();
    expect(screen.getByText('run #2 failed — ran out')).toBeInTheDocument();
    expect(screen.getAllByText('@dev-1').length).toBeGreaterThan(0);
    expect(screen.getAllByText('you').length).toBeGreaterThan(0);
  });

  it('says so when there is no history rather than rendering a blank', () => {
    renderTimeline([]);
    expect(screen.getByText('Nothing has happened yet.')).toBeInTheDocument();
  });

  it('sends trimmed text and clears the box', async () => {
    const onComment = renderTimeline([]);
    const box = screen.getByPlaceholderText('Say something about this issue…');

    await userEvent.type(box, '   look at the retry path   ');
    await userEvent.click(screen.getByRole('button', { name: 'Comment' }));

    expect(onComment).toHaveBeenCalledWith('look at the retry path');
    expect(box).toHaveValue('');
  });

  it('refuses to send whitespace', async () => {
    const onComment = renderTimeline([]);
    await userEvent.type(screen.getByPlaceholderText('Say something about this issue…'), '   ');
    expect(screen.getByRole('button', { name: 'Comment' })).toBeDisabled();
    expect(onComment).not.toHaveBeenCalled();
  });

  it('tells the operator what sending will actually do', async () => {
    renderTimeline([]);
    expect(screen.getByText(/Starts a run/)).toBeInTheDocument();
  });
});

describe('pending approvals', () => {
  it('offers all three answers and reports which one was clicked', async () => {
    const onResolveApproval = vi.fn();
    render(
      <Timeline
        events={[
          entry({
            kind: 'approval_requested',
            call_id: 'c1',
            tool: 'Bash',
            summary: 'rm -rf build',
          }),
        ]}
        issue={ISSUE}
        runs={[]}
        onComment={vi.fn()}
        onResolveApproval={onResolveApproval}
        busy={false}
      />,
    );
    expect(screen.getByText(/Waiting on you/)).toBeInTheDocument();
    expect(screen.getByText('rm -rf build')).toBeInTheDocument();

    await userEvent.click(screen.getByRole('button', { name: 'Approve always' }));
    expect(onResolveApproval).toHaveBeenCalledWith('c1', 'approve_always');
    await userEvent.click(screen.getByRole('button', { name: 'Deny' }));
    expect(onResolveApproval).toHaveBeenCalledWith('c1', 'deny');
  });

  it('stops offering a prompt that has been answered', () => {
    render(
      <Timeline
        events={[
          entry({
            kind: 'approval_requested',
            call_id: 'c1',
            tool: 'Bash',
            summary: 'rm -rf build',
          }),
          entry({ kind: 'approval_resolved', call_id: 'c1', decision: 'deny' }),
        ]}
        issue={ISSUE}
        runs={[]}
        onComment={vi.fn()}
        onResolveApproval={vi.fn()}
        busy={false}
      />,
    );
    expect(screen.queryByText(/Waiting on you/)).not.toBeInTheDocument();
    // The decision freezes in place rather than collapsing to a sentence:
    // "who let this through, and when" is what a reader comes back for.
    expect(screen.getByText(/Denied/)).toBeInTheDocument();
  });

  it('marks the operator’s own comment apart from an agent’s report', () => {
    const { container } = render(
      <Timeline
        events={[
          { ...entry({ kind: 'comment', text: 'do the thing' }), actor: { kind: 'user' } },
          {
            ...entry({ kind: 'comment', text: 'done' }),
            id: 'e9',
            actor: { kind: 'agent', id: '01JA', handle: 'dev-1' },
          },
        ]}
        issue={ISSUE}
        runs={[]}
        onComment={vi.fn()}
        onResolveApproval={vi.fn()}
        busy={false}
      />,
    );
    // The user's bubble carries the chat page's amber; the agent's is
    // surface. Identical bubbles made the two indistinguishable.
    const bubbles = container.querySelectorAll('li > div');
    expect(bubbles[0].className).toContain('bg-brand');
    expect(bubbles[1].className).toContain('bg-surface');
  });

  it('renders a comment’s markdown rather than its source', () => {
    render(
      <Timeline
        events={[entry({ kind: 'comment', text: 'the **root cause** is a timer' })]}
        issue={ISSUE}
        runs={[]}
        onComment={vi.fn()}
        onResolveApproval={vi.fn()}
        busy={false}
      />,
    );
    expect(screen.getByText('root cause').tagName).toBe('STRONG');
  });
});