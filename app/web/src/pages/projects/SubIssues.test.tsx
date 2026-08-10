import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter } from 'react-router-dom';

import { SubIssues } from './SubIssues';
import type { Agent, Issue } from './boardModel';


let seq = 0;
function child(stage: number, overrides: Partial<Issue> = {}): Issue {
  seq += 1;
  return {
    number: seq,
    project_id: '01JP',
    title: `step ${seq}`,
    description: '',
    status: 'backlog',
    priority: 'none',
    position: 0,
    stage,
    created_at_ms: 0,
    updated_at_ms: 0,
    ...overrides,
  };
}

const TEAM: Agent[] = [
  {
    id: 'id-dev',
    handle: 'dev-1',
    name: 'Dev',
    description: '',
    framework: 'baybo',
    lead: false,
    created_at_ms: 0,
  },
];

function renderSteps(children: Issue[], disabled = false) {
  const onStatus = vi.fn();
  render(
    <MemoryRouter>
      <SubIssues
        projectId="01JP"
        children={children}
        team={TEAM}
        disabled={disabled}
        onStatus={onStatus}
      onAssignee={vi.fn()}
      />
    </MemoryRouter>,
  );
  return onStatus;
}

describe('SubIssues', () => {
  it('renders nothing for a card with no steps', () => {
    const { container } = render(
      <MemoryRouter>
        <SubIssues
          projectId="01JP"
          children={[]}
          team={TEAM}
          disabled={false}
          onStatus={vi.fn()}
          onAssignee={vi.fn()}
        />
      </MemoryRouter>,
    );
    expect(container).toBeEmptyDOMElement();
  });

  it('makes the barrier visible: only one stage is in progress', () => {
    renderSteps([child(0, { status: 'done' }), child(1), child(2)]);
    expect(screen.getByText('finished')).toBeInTheDocument();
    expect(screen.getByText('in progress')).toBeInTheDocument();
    expect(screen.getByText(/starts when the stage above finishes/)).toBeInTheDocument();
  });

  it('counts each stage on its own, and only work still meant to happen', () => {
    renderSteps([
      child(0, { status: 'done' }),
      child(0, { status: 'done' }),
      child(1, { cancelled_at_ms: 1 }),
    ]);
    // Stage 0 is finished; stage 1 holds one cancelled step, which leaves
    // both sides of the count — it is neither work done nor work owed.
    expect(screen.getByText('2/2 done')).toBeInTheDocument();
    expect(screen.getByText('0/0 done')).toBeInTheDocument();
  });

  it('moves a step in place and shows who is on it', async () => {
    const step = child(1, { assignee: 'id-dev' });
    const onStatus = renderSteps([step]);
    // A face and a status pill, as the mockup's row has them — the pickers
    // are laid over them rather than replacing them with form widgets.
    expect(screen.getByTitle('@dev-1')).toBeInTheDocument();
    expect(screen.getByText('BACKLOG')).toBeInTheDocument();

    await userEvent.selectOptions(
      screen.getByLabelText(`Status of #${step.number}`),
      'in_progress',
    );
    expect(onStatus).toHaveBeenCalledWith(step.number, 'in_progress');
  });

  it('shows an empty seat rather than nothing when a step has nobody on it', () => {
    const step = child(1);
    renderSteps([step]);
    expect(screen.getByTitle('Unassigned')).toBeInTheDocument();
    expect(screen.getByLabelText(`Assignee of #${step.number}`)).toHaveValue('');
  });

  it('does not offer to move a cancelled step or anything while saving', () => {
    const cancelled = child(0, { cancelled_at_ms: 1 });
    renderSteps([cancelled]);
    expect(screen.getByLabelText(`Status of #${cancelled.number}`)).toBeDisabled();

    const live = child(0);
    renderSteps([live], true);
    expect(screen.getByLabelText(`Status of #${live.number}`)).toBeDisabled();
  });
});
