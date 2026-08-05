import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter } from 'react-router-dom';

import { AgentProfile } from './AgentProfile';
import type { Agent, Issue, IssueRun } from './boardModel';

function agent(handle: string, overrides: Partial<Agent> = {}): Agent {
  return {
    id: `id-${handle}`,
    handle,
    name: handle.toUpperCase(),
    description: `the ${handle}`,
    framework: 'baybo',
    lead: false,
    created_at_ms: Date.UTC(2026, 0, 2),
    ...overrides,
  };
}

let seq = 0;
function issue(overrides: Partial<Issue> = {}): Issue {
  seq += 1;
  return {
    number: seq,
    project_id: '01JP',
    title: `card ${seq}`,
    description: '',
    status: 'todo',
    priority: 'none',
    position: 0,
    stage: 0,
    created_at_ms: 0,
    updated_at_ms: 0,
    ...overrides,
  };
}

function renderProfile(a: Agent, issues: Issue[] = [], runs: IssueRun[] = [], readOnly = false) {
  const onRemove = vi.fn();
  render(
    <MemoryRouter>
      <AgentProfile
        agent={a}
        team={[a]}
        issues={issues}
        activeRuns={runs}
        readOnly={readOnly}
        projectId="01JP"
        onClose={vi.fn()}
        onRemove={onRemove}
      />
    </MemoryRouter>,
  );
  return onRemove;
}

describe('AgentProfile', () => {
  it('shows only the live cards this agent is on', () => {
    renderProfile(agent('dev-1'), [
      issue({ assignee: 'id-dev-1', title: 'mine' }),
      issue({ assignee: 'id-dev-1', title: 'called off', cancelled_at_ms: 1 }),
      issue({ assignee: 'id-other', title: 'somebody else’s' }),
    ]);
    expect(screen.getByText('mine')).toBeInTheDocument();
    expect(screen.queryByText('called off')).not.toBeInTheDocument();
    expect(screen.queryByText('somebody else’s')).not.toBeInTheDocument();
    expect(screen.getByText('1')).toBeInTheDocument();
  });

  it('says who added it, and notes a hirer who has since gone', () => {
    renderProfile(agent('qa'));
    expect(screen.getByText(/you added it/)).toBeInTheDocument();

    // The hirer is resolved server-side precisely because it may no longer
    // be on the roster, so the panel must render that case rather than a
    // blank.
    renderProfile(agent('qa-2', { hired_by: { id: 'id-gone', handle: 'old-lead' } }));
    expect(screen.getByText(/hired by @old-lead \(since removed\)/)).toBeInTheDocument();
  });

  it('offers no removal for the lead, and says why', () => {
    renderProfile(agent('lead', { lead: true }));
    expect(screen.queryByRole('button', { name: /Remove from project/ })).not.toBeInTheDocument();
    expect(screen.getByText(/cannot be removed/)).toBeInTheDocument();
  });

  it('offers no removal on an archived board', () => {
    renderProfile(agent('dev-1'), [], [], true);
    expect(screen.queryByRole('button', { name: /Remove from project/ })).not.toBeInTheDocument();
  });

  it('removes an ordinary teammate', async () => {
    const onRemove = renderProfile(agent('dev-1'));
    await userEvent.click(screen.getByRole('button', { name: /Remove from project/ }));
    expect(onRemove).toHaveBeenCalledWith(expect.objectContaining({ handle: 'dev-1' }));
  });
});
