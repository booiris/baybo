import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { botttsFace } from '../../components/botttsFace';
import { Timeline } from './Timeline';
import type { Agent, Issue } from './boardModel';

// The composer uploads attachments, so it reads the operator's bearer. These
// tests are about what the timeline renders, not about the network.
vi.mock('../../api/auth', () => ({
  useAdminClient: () => ({}),
  useAuth: () => ({ token: 't', baseUrl: 'http://gw', logout: vi.fn() }),
}));

const TEAM: Agent[] = [
  {
    id: '01JDEV',
    handle: 'dev-1',
    name: 'Dev One',
    description: '',
    framework: 'baybo',
    lead: false,
    created_at_ms: 0,
  },
];
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
  unread: 0,
  last_run_failed: false,
  pinned: false,
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

  it('offers teammates as you type an @, and takes the one you pick', async () => {
    const onComment = vi.fn();
    render(
      <Timeline
        events={[]}
        issue={ISSUE}
        runs={[]}
        onComment={onComment}
        onResolveApproval={vi.fn()}
        team={TEAM}
        busy={false}
      />,
    );
    const box = screen.getByPlaceholderText(/^Comment…/);
    await userEvent.type(box, 'ask @de');

    await userEvent.click(screen.getByRole('button', { name: /@dev-1/ }));
    expect(box).toHaveValue('ask @dev-1 ');
    // The field keeps focus, or the caret would jump back to the start.
    expect(box).toHaveFocus();
  });

  it('closes the picker on Escape without touching the draft', async () => {
    render(
      <Timeline
        events={[]}
        issue={ISSUE}
        runs={[]}
        onComment={vi.fn()}
        onResolveApproval={vi.fn()}
        team={TEAM}
        busy={false}
      />,
    );
    const box = screen.getByPlaceholderText(/^Comment…/);
    await userEvent.type(box, 'ask @de');
    expect(screen.getByRole('button', { name: /@dev-1/ })).toBeInTheDocument();

    await userEvent.type(box, '{Escape}');
    expect(screen.queryByRole('button', { name: /@dev-1/ })).toBeNull();
    expect(box).toHaveValue('ask @de');
  });

  it('sends trimmed text and clears the box', async () => {
    const onComment = renderTimeline([]);
    const box = screen.getByPlaceholderText(/^Comment…/);

    await userEvent.type(box, '   look at the retry path   ');
    await userEvent.click(screen.getByRole('button', { name: 'Comment' }));

    expect(onComment).toHaveBeenCalledWith('look at the retry path', []);
    expect(box).toHaveValue('');
  });

  it('refuses to send whitespace', async () => {
    const onComment = renderTimeline([]);
    await userEvent.type(screen.getByPlaceholderText(/^Comment…/), '   ');
    expect(screen.getByRole('button', { name: 'Comment' })).toBeDisabled();
    expect(onComment).not.toHaveBeenCalled();
  });

  it('tells the operator what sending will actually do', () => {
    renderTimeline([]);
    // The sentence lost its chip but not its job: whether sending spends
    // money or only records is on the button you are about to press.
    expect(screen.getByRole('button', { name: 'Comment' })).toHaveAttribute(
      'title',
      expect.stringContaining('Starts a run') as unknown as string,
    );
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
    // The command is what is being approved, so it gets its own box rather
    // than sharing a paragraph with the ask.
    expect(screen.getByText(/Bash · rm -rf build/)).toBeInTheDocument();

    await userEvent.click(screen.getByRole('button', { name: 'Approve always' }));
    expect(onResolveApproval).toHaveBeenCalledWith('c1', 'approve_always');
    await userEvent.click(screen.getByRole('button', { name: 'Deny' }));
    expect(onResolveApproval).toHaveBeenCalledWith('c1', 'deny');
  });

  it('asks where it happened rather than above the whole history', () => {
    // Hoisting every open prompt to the top of the timeline detached the
    // decision from the run that provoked it.
    const { container } = render(
      <Timeline
        events={[
          entry({ kind: 'moved', from: 'todo', to: 'in_progress' }),
          entry({ kind: 'approval_requested', call_id: 'c2', tool: 'Bash', summary: 'git push' }),
        ]}
        issue={ISSUE}
        runs={[]}
        onComment={vi.fn()}
        onResolveApproval={vi.fn()}
        busy={false}
      />,
    );
    const rows = [...container.querySelectorAll('li')];
    expect(rows[0].textContent).toContain('moved it from Todo');
    expect(rows[1].textContent).toContain('Waiting on you');
  });

  it('stops offering a prompt that has been answered, and keeps what was answered', () => {
    render(
      <Timeline
        events={[
          entry({
            kind: 'approval_requested',
            call_id: 'c1',
            tool: 'Bash',
            summary: 'rm -rf build',
          }),
          entry({
            kind: 'approval_resolved',
            call_id: 'c1',
            decision: 'deny',
            resolution: 'answered',
          }),
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
    // "who let this through, what, and when" is what a reader comes back
    // for — and a verdict with the command stripped off answers none of it.
    expect(screen.getByText(/Denied/)).toBeInTheDocument();
    expect(screen.getByText(/Bash · rm -rf build/)).toBeInTheDocument();
    // …and the ask is not narrated a second time above its own verdict.
    expect(screen.getAllByText(/Bash · rm -rf build/)).toHaveLength(1);
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
    const bubbles = container.querySelectorAll('.chat-prose');
    expect(bubbles[0].className).toContain('bg-brand');
    expect(bubbles[1].className).toContain('bg-surface');
  });

  it('gives each speaker a face, and marks the operator’s own', () => {
    render(
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
    expect(screen.getByTitle('you')).toBeInTheDocument();
    // An agent nobody has uploaded a portrait for still gets a face of its
    // own, generated from its id. The operator is not an agent and keeps
    // initials.
    expect(screen.getByTitle('@dev-1').querySelector('img')?.getAttribute('src')).toBe(
      botttsFace('01JA'),
    );
    expect(screen.getByTitle('you').querySelector('img')).toBeNull();
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