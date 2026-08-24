import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter } from 'react-router-dom';

import { setAgentModel } from './api';
import { AgentProfile } from './AgentProfile';
import type { Agent, Issue, IssueRun } from './boardModel';

vi.mock('../../api/auth', () => ({
  useAdminClient: () => ({}),
  useAuth: () => ({ token: 't', baseUrl: 'http://x', logout: vi.fn() }),
}));
// The model pool is a network read the panel makes on mount; the tests
// here are about what it renders, not about which models exist.
vi.mock('./api', () => ({
  fetchModelPool: vi.fn().mockResolvedValue({
    kind: 'ok',
    value: {
      defaultName: 'deepseek',
      entries: [
        { name: 'deepseek', models: ['deepseek-chat'], efforts: [] },
        { name: 'gpt-5', models: ['gpt-5.5', 'o3'], efforts: ['low', 'high'] },
      ],
    },
  }),
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
    unread: 0,
    last_run_failed: false,
    approval_pending: false,
    opened_by_agent: false,
    pinned: false,
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

  /// Rows of whichever picker is open. The trigger carries `aria-haspopup`;
  /// everything else inside the panel is a row.
  function openRows() {
    return screen
      .getAllByRole('button')
      .filter((one) => !one.hasAttribute('aria-haspopup') && one.closest('[style]') !== null)
      .map((one) => one.textContent);
  }

  it('offers an inherit row plus every entry, the current default named on it', async () => {
    renderProfile(agent('dev-1'));
    // An unpinned agent sits on the inherit row, which says what it resolves
    // to today without pinning it.
    const trigger = await screen.findByLabelText('llm: Default · deepseek');
    await userEvent.click(trigger);

    // The default entry keeps its own named row: picking it by name is the
    // only way to then choose a model inside it.
    expect(openRows()).toEqual(['Default · deepseek', 'deepseek', 'gpt-5']);
  });

  it('offers the pinned entry’s models, and its rungs when it has any', async () => {
    renderProfile(agent('dev-1', { llm: 'gpt-5' }));

    await userEvent.click(await screen.findByLabelText('model: gpt-5.5 (entry default)'));
    expect(openRows()).toEqual(['gpt-5.5 (entry default)', 'gpt-5.5', 'o3']);

    await userEvent.click(screen.getByLabelText('thinking: entry default'));
    expect(openRows()).toEqual(['entry default', 'low', 'high']);
  });

  /// An entry whose provider baybo sends no effort to gets no field at all —
  /// a disabled row would advertise a knob that does not exist.
  it('draws no thinking field for a provider that takes no effort', async () => {
    renderProfile(agent('dev-1', { llm: 'deepseek' }));
    await screen.findByLabelText('llm: deepseek');
    expect(screen.queryByLabelText(/^thinking:/)).not.toBeInTheDocument();
  });

  /// The pin is written whole. Changing the entry drops the model with it,
  /// because a model of the old entry is one the new entry cannot serve.
  it('writes the whole pin, and clears the model when the entry moves', async () => {
    renderProfile(agent('dev-1', { llm: 'gpt-5', model: 'o3', reasoning_effort: 'high' }));

    await userEvent.click(await screen.findByLabelText('llm: gpt-5'));
    await userEvent.click(screen.getByRole('button', { name: 'deepseek' }));

    expect(vi.mocked(setAgentModel)).toHaveBeenCalledWith({}, 'id-dev-1', {
      llm: 'deepseek',
      model: '',
      effort: 'high',
    });
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
