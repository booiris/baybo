import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { TeamStrip } from './TeamStrip';
import type { Agent, IssueRun } from './boardModel';

/// The board resolves faces once and hands them down; the strip must not go
/// looking for its own.
const portrait = (agentId: string | null | undefined) =>
  agentId == null ? null : `data:face/${agentId}`;

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



function member(handle: string, lead = false): Agent {
  return {
    id: `id-${handle}`,
    handle,
    name: handle,
    description: `the ${handle}`,
    framework: 'baybo',
    lead,
    created_at_ms: 0,
  };
}

const TEAM: Agent[] = [member('lead', true), member('dev-1')];

function renderStrip(
  overrides: Partial<Parameters<typeof TeamStrip>[0]> = {},
): {
  onHire: ReturnType<typeof vi.fn>;
  onOpenProfile: ReturnType<typeof vi.fn>;
} {
  const onHire = vi.fn().mockResolvedValue(null);
  const onOpenProfile = vi.fn();
  render(
    <TeamStrip
      team={TEAM}
      activeRuns={[]}
      portrait={portrait}
      readOnly={false}
      onHire={onHire}
      onOpenProfile={onOpenProfile}
      {...overrides}
    />,
  );
  return { onHire, onOpenProfile };
}

describe('TeamStrip', () => {
  it('draws a face for every member, from the board’s own lookup', () => {
    renderStrip();
    // Faces, not named pills: sixteen handles is wider than the header, and
    // the avatar is what the operator already recognises on every card.
    for (const handle of ['lead', 'dev-1']) {
      const seat = screen.getByRole('button', { name: `Open @${handle}'s profile` });
      expect(seat.querySelector('img')).toHaveAttribute('src', `data:face/id-${handle}`);
    }
    expect(screen.queryByText('@dev-1')).not.toBeInTheDocument();
  });

  it('opens a profile from any handle, including the lead\'s', async () => {
    const { onOpenProfile } = renderStrip();
    await userEvent.click(screen.getByRole('button', { name: /Open @lead's profile/ }));
    expect(onOpenProfile).toHaveBeenCalledWith(expect.objectContaining({ handle: 'lead' }));
  });

  it('offers no writes on an archived board', () => {
    renderStrip({ readOnly: true });
    expect(screen.getByRole('button', { name: /Open @dev-1's profile/ })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /Add an agent/ })).not.toBeInTheDocument();
  });

  it('marks the agents that are running, and not the queued ones', () => {
    const runs: IssueRun[] = [
      {
        number: 1,
        agent_id: 'id-dev-1',
        trigger: 'started',
        status: 'queued',
        attempt: 1,
        created_at_ms: 0,
      },
    ];
    const { rerender } = render(
      <TeamStrip
        team={TEAM}
        activeRuns={runs}
        portrait={portrait}
        readOnly
        onHire={vi.fn()}
        onOpenProfile={vi.fn()}
      />,
    );
    expect(screen.queryByTitle(/@dev-1\) — working/)).not.toBeInTheDocument();

    rerender(
      <TeamStrip
        team={TEAM}
        activeRuns={[{ ...runs[0], status: 'running' }]}
        portrait={portrait}
        readOnly
        onHire={vi.fn()}
        onOpenProfile={vi.fn()}
      />,
    );
    expect(screen.getByTitle(/@dev-1\) — working/)).toBeInTheDocument();
  });

  it('keeps the form open and shows why when a hire is refused', async () => {
    const onHire = vi.fn().mockResolvedValue('@qa and every numbered variant are taken');
    render(
      <TeamStrip
        team={TEAM}
        activeRuns={[]}
        portrait={portrait}
        readOnly={false}
        onHire={onHire}
        onOpenProfile={vi.fn()}
      />,
    );
    await userEvent.click(screen.getByRole('button', { name: /Add an agent/ }));

    const submit = screen.getByRole('button', { name: 'Create agent' });
    expect(submit).toBeDisabled();
    await userEvent.type(screen.getByLabelText(/Name/), 'qa');
    expect(submit).toBeDisabled();
    await userEvent.type(screen.getByLabelText(/Role/), 'Tests things.');
    await userEvent.click(submit);

    // The user form carries framework and LLM pin — the two knobs the lead's
    // own hiring tool deliberately does not get.
    expect(onHire).toHaveBeenCalledWith({
      name: 'qa',
      role: 'Tests things.',
      framework: 'baybo',
    });
    expect(await screen.findByText(/every numbered variant are taken/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Create agent' })).toBeInTheDocument();
  });

  it('closes on Escape as well as on ✕ and the backdrop', async () => {
    renderStrip();
    await userEvent.click(screen.getByRole('button', { name: /Add an agent/ }));
    expect(screen.getByRole('button', { name: 'Create agent' })).toBeInTheDocument();

    await userEvent.keyboard('{Escape}');
    expect(screen.queryByRole('button', { name: 'Create agent' })).not.toBeInTheDocument();
  });

  it('refuses a name that cannot be a handle, before asking the server', async () => {
    renderStrip();
    await userEvent.click(screen.getByRole('button', { name: /Add an agent/ }));
    await userEvent.type(screen.getByLabelText(/Name/), 'Test Engineer');

    // The name *is* the handle now, so there is no slug to preview — there is
    // a rule, and the field says which part of it was broken.
    expect(screen.getByText(/lowercase letter/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Create agent' })).toBeDisabled();
  });

  it('picks framework and llm by press, not by an OS menu', async () => {
    const { onHire } = renderStrip();
    await userEvent.click(screen.getByRole('button', { name: /Add an agent/ }));
    await userEvent.type(screen.getByLabelText(/Name/), 'qa');
    await userEvent.type(screen.getByLabelText(/Role/), 'Tests things.');

    // Both fields name what they are set to, and open the board's own panel.
    await userEvent.click(screen.getByLabelText('Framework: native'));
    await userEvent.click(screen.getByRole('button', { name: 'codex' }));
    await userEvent.click(screen.getByLabelText('llm: deepseek'));
    await userEvent.click(screen.getByRole('button', { name: 'gpt-5' }));

    await userEvent.click(screen.getByRole('button', { name: 'Create agent' }));
    expect(onHire).toHaveBeenCalledWith({
      name: 'qa',
      role: 'Tests things.',
      framework: 'codex',
      llm: 'gpt-5',
    });
  });

  it('closes the form once the hire lands', async () => {
    const { onHire } = renderStrip();
    await userEvent.click(screen.getByRole('button', { name: /Add an agent/ }));
    await userEvent.type(screen.getByLabelText(/Name/), 'qa');
    await userEvent.type(screen.getByLabelText(/Role/), 'Tests things.');
    await userEvent.click(screen.getByRole('button', { name: 'Create agent' }));

    expect(onHire).toHaveBeenCalled();
    expect(screen.queryByRole('button', { name: 'Create agent' })).not.toBeInTheDocument();
  });
});
