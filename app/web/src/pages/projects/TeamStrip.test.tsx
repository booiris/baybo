import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { TeamStrip } from './TeamStrip';
import type { Agent, IssueRun } from './boardModel';


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
  onRemove: ReturnType<typeof vi.fn>;
  onOpenProfile: ReturnType<typeof vi.fn>;
} {
  const onHire = vi.fn().mockResolvedValue(null);
  const onRemove = vi.fn();
  const onOpenProfile = vi.fn();
  render(
    <TeamStrip
      team={TEAM}
      activeRuns={[]}
      readOnly={false}
      onHire={onHire}
      onRemove={onRemove}
      onOpenProfile={onOpenProfile}
      {...overrides}
    />,
  );
  return { onHire, onRemove, onOpenProfile };
}

describe('TeamStrip', () => {
  it('shows every handle and lets only non-leads be removed', async () => {
    const { onRemove } = renderStrip();
    expect(screen.getByText('@lead')).toBeInTheDocument();
    expect(screen.getByText('@dev-1')).toBeInTheDocument();

    expect(
      screen.queryByRole('button', { name: /Remove @lead/ }),
    ).not.toBeInTheDocument();

    await userEvent.click(screen.getByRole('button', { name: /Remove @dev-1/ }));
    expect(onRemove).toHaveBeenCalledWith(expect.objectContaining({ handle: 'dev-1' }));
  });

  it('opens a profile from any handle, including the lead\'s', async () => {
    const { onOpenProfile } = renderStrip();
    await userEvent.click(screen.getByRole('button', { name: /Open @lead's profile/ }));
    expect(onOpenProfile).toHaveBeenCalledWith(expect.objectContaining({ handle: 'lead' }));
  });

  it('offers no writes on an archived board', () => {
    renderStrip({ readOnly: true });
    expect(screen.getByText('@dev-1')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /Add an agent/ })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /Remove/ })).not.toBeInTheDocument();
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
        readOnly
        onHire={vi.fn()}
        onRemove={vi.fn()}
        onOpenProfile={vi.fn()}
      />,
    );
    expect(screen.queryByTitle(/@dev-1\) — working/)).not.toBeInTheDocument();

    rerender(
      <TeamStrip
        team={TEAM}
        activeRuns={[{ ...runs[0], status: 'running' }]}
        readOnly
        onHire={vi.fn()}
        onRemove={vi.fn()}
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
        readOnly={false}
        onHire={onHire}
        onRemove={vi.fn()}
        onOpenProfile={vi.fn()}
      />,
    );
    await userEvent.click(screen.getByRole('button', { name: /Add an agent/ }));

    const submit = screen.getByRole('button', { name: 'Add agent' });
    expect(submit).toBeDisabled();
    await userEvent.type(screen.getByLabelText(/Name/), 'QA');
    expect(submit).toBeDisabled();
    await userEvent.type(screen.getByLabelText(/Role/), 'Tests things.');
    await userEvent.click(submit);

    expect(onHire).toHaveBeenCalledWith({ name: 'QA', role: 'Tests things.' });
    expect(await screen.findByText(/every numbered variant are taken/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Add agent' })).toBeInTheDocument();
  });

  it('closes the form once the hire lands', async () => {
    const { onHire } = renderStrip();
    await userEvent.click(screen.getByRole('button', { name: /Add an agent/ }));
    await userEvent.type(screen.getByLabelText(/Name/), 'QA');
    await userEvent.type(screen.getByLabelText(/Role/), 'Tests things.');
    await userEvent.click(screen.getByRole('button', { name: 'Add agent' }));

    expect(onHire).toHaveBeenCalled();
    expect(screen.queryByRole('button', { name: 'Add agent' })).not.toBeInTheDocument();
  });
});
