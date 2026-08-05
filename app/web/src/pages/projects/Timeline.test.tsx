import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { Timeline } from './Timeline';
import type { Issue } from './boardModel';
import type { IssueEvent, IssueEventBody } from './timelineModel';

// The wiring a reducer test can't reach: that comments and system notes
// render as two different things, and that the composer sends what the
// operator meant rather than what they typed.

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
function entry(body: IssueEventBody, agent?: string): IssueEvent {
  seq += 1;
  return {
    id: `evt-${seq}`,
    number: 4,
    actor: agent ?? 'user',
    actor_is_agent: agent != null,
    body,
    created_at_ms: Date.now(),
  };
}

function renderTimeline(events: IssueEvent[], onComment = vi.fn(), busy = false) {
  render(
    <Timeline events={events} issue={ISSUE} runs={[]} onComment={onComment} busy={busy} />,
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
    // A comment is shown verbatim, not narrated.
    expect(screen.getByText('check the reconnect path')).toBeInTheDocument();
    // …and a run's failure carries its reason where a reader will see it.
    expect(screen.getByText('run #2 failed — ran out')).toBeInTheDocument();
    // The operator and the agent are told apart.
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
    // The expensive failure is silent: someone comments believing an agent
    // will read it, and waits for an answer nobody is going to send.
    renderTimeline([]);
    expect(screen.getByText(/Starts a run/)).toBeInTheDocument();
  });
});
