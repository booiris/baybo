import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter } from 'react-router-dom';

import { AgentProfile } from './AgentProfile';
import type { Agent, Issue, IssueRun } from './boardModel';

vi.mock('../../api/auth', () => ({
  useAdminClient: () => ({}),
  useAuth: () => ({ token: 't', baseUrl: 'http://x', logout: vi.fn() }),
}));
// The model pool is a network read the panel makes on mount; the tests
// here are about what it renders, not about which models exist.
vi.mock('./api', () => ({
  fetchModelPool: vi
    .fn()
    .mockResolvedValue({ kind: 'ok', value: { names: ['deepseek', 'gpt-5'], defaultName: 'deepseek' } }),
  setAgentModel: vi.fn().mockResolvedValue({ kind: 'ok', value: null }),
}));


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
      onChanged={vi.fn()}
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

  it('lists each model once, the default one standing for "not pinned"', async () => {
    renderProfile(agent('dev-1'));
    // The trigger names what it is set to — an unpinned agent shows the
    // default's own row, because that row *is* the unpinned choice.
    const trigger = await screen.findByLabelText('llm: deepseek');
    await userEvent.click(trigger);

    // No separate "default" row beside the model it resolves to: the two are
    // indistinguishable until the default moves, and reading `deepseek` twice
    // is worse than losing that distinction.
    // The trigger carries `aria-haspopup`; everything else here is a row.
    const rows = screen
      .getAllByRole('button')
      .filter((one) => !one.hasAttribute('aria-haspopup') && one.closest('[style]') !== null)
      .map((one) => one.textContent);
    expect(rows).toEqual(['deepseek', 'gpt-5']);
  });

  it('says who added it, and notes a hirer who has since gone', () => {
    renderProfile(agent('qa'));
    expect(screen.getByText(/you added it/)).toBeInTheDocument();

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

  it('asks before it tombstones a teammate', async () => {
    const onRemove = renderProfile(agent('dev-1'));
    await userEvent.click(screen.getByRole('button', { name: /Remove from project/ }));
    // One click is not enough: removal is a tombstone — the handle stays
    // reserved for good and every past entry keeps naming this agent.
    expect(onRemove).not.toHaveBeenCalled();
    expect(screen.getByText(/no undo/)).toBeInTheDocument();

    await userEvent.click(screen.getByRole('button', { name: 'Remove' }));
    expect(onRemove).toHaveBeenCalledWith(expect.objectContaining({ handle: 'dev-1' }));
  });

  it('lets a mis-click back out', async () => {
    const onRemove = renderProfile(agent('dev-1'));
    await userEvent.click(screen.getByRole('button', { name: /Remove from project/ }));
    await userEvent.click(screen.getByRole('button', { name: 'Keep' }));
    expect(onRemove).not.toHaveBeenCalled();
  });
});
