import { describe, expect, it, vi, beforeEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { ProjectSettings } from './ProjectSettings';
import type { Project } from './boardModel';


const api = vi.hoisted(() => ({ updateProject: vi.fn(), setProjectArchived: vi.fn() }));
vi.mock('./api', () => api);
const client = vi.hoisted(() => ({}));
const auth = vi.hoisted(() => ({ logout: () => {} }));
vi.mock('../../api/auth', () => ({ useAdminClient: () => client, useAuth: () => auth }));

function project(overrides: Partial<Project> = {}): Project {
  return {
    id: '01JP',
    name: 'Kanban',
    description: 'the board',
    workdir: '/tmp/kanban',
    max_parallel_issue_runs: 3,
    created_at_ms: 0,
    updated_at_ms: 0,
    ...overrides,
  };
}

describe('ProjectSettings', () => {
  beforeEach(() => {
    api.updateProject.mockReset().mockImplementation(async (_c, _id, body) => ({
      kind: 'ok',
      value: project(body),
    }));
    api.setProjectArchived.mockReset().mockResolvedValue({ kind: 'ok', value: project() });
  });

  it('shows a stored ceiling as dollars and saves it back as micro-USD', async () => {
    render(
      <ProjectSettings
        project={project({ daily_budget_micros: 12_500_000 })}
        onClose={vi.fn()}
        onSaved={vi.fn()}
      />,
    );
    const box = screen.getByLabelText('Daily budget');
    expect(box).toHaveValue('12.5');

    await userEvent.clear(box);
    await userEvent.type(box, '20');
    await userEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => {
      expect(api.updateProject).toHaveBeenCalledWith(
        client,
        '01JP',
        expect.objectContaining({ daily_budget_micros: 20_000_000 }),
      );
    });
  });

  it('sends null when the box is emptied, so a ceiling can be removed', async () => {
    render(
      <ProjectSettings
        project={project({ daily_budget_micros: 5_000_000 })}
        onClose={vi.fn()}
        onSaved={vi.fn()}
      />,
    );
    await userEvent.clear(screen.getByLabelText('Daily budget'));
    await userEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => {
      expect(api.updateProject).toHaveBeenCalledWith(
        client,
        '01JP',
        expect.objectContaining({ daily_budget_micros: null }),
      );
    });
  });

  it('shows the run ceiling and saves the number back', async () => {
    render(
      <ProjectSettings
        project={project({ max_parallel_issue_runs: 2 })}
        onClose={vi.fn()}
        onSaved={vi.fn()}
      />,
    );
    const box = screen.getByLabelText('Parallel issue runs');
    expect(box).toHaveValue('2');

    await userEvent.clear(box);
    await userEvent.type(box, '5');
    await userEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => {
      expect(api.updateProject).toHaveBeenCalledWith(
        client,
        '01JP',
        expect.objectContaining({ max_parallel_issue_runs: 5 }),
      );
    });
  });

  it('says what zero means, because it is the off switch and not a blank', async () => {
    render(
      <ProjectSettings
        project={project({ max_parallel_issue_runs: 0 })}
        onClose={vi.fn()}
        onSaved={vi.fn()}
      />,
    );
    expect(screen.getByText(/cards stay in Todo until you move them/)).toBeInTheDocument();
  });

  it('refuses an empty run ceiling rather than silently restoring the default', async () => {
    render(<ProjectSettings project={project()} onClose={vi.fn()} onSaved={vi.fn()} />);
    await userEvent.clear(screen.getByLabelText('Parallel issue runs'));
    await userEvent.click(screen.getByRole('button', { name: 'Save' }));
    expect(await screen.findByText(/must be a whole number/)).toBeInTheDocument();
    expect(api.updateProject).not.toHaveBeenCalled();
  });

  it('separates zero from empty in what it tells the operator', () => {
    render(<ProjectSettings project={project()} onClose={vi.fn()} onSaved={vi.fn()} />);
    expect(screen.getByText(/No ceiling/)).toBeInTheDocument();
  });

  it('refuses text that is not an amount instead of sending something else', async () => {
    render(<ProjectSettings project={project()} onClose={vi.fn()} onSaved={vi.fn()} />);
    await userEvent.type(screen.getByLabelText('Daily budget'), 'lots');
    await userEvent.click(screen.getByRole('button', { name: 'Save' }));
    expect(await screen.findByText(/must be an amount in dollars/)).toBeInTheDocument();
    expect(api.updateProject).not.toHaveBeenCalled();
  });

  it('makes an archived board read-only but still un-archivable', async () => {
    render(
      <ProjectSettings
        project={project({ archived_at_ms: 111 })}
        onClose={vi.fn()}
        onSaved={vi.fn()}
      />,
    );
    expect(screen.getByLabelText('Name')).toBeDisabled();
    expect(screen.getByLabelText('Daily budget')).toBeDisabled();
    expect(screen.queryByRole('button', { name: 'Save' })).not.toBeInTheDocument();

    await userEvent.click(screen.getByRole('button', { name: 'Unarchive' }));
    expect(api.setProjectArchived).toHaveBeenCalledWith(client, '01JP', false);
  });
});
