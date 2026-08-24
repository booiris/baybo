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

// Milkdown drives a real ProseMirror view, which jsdom hosts badly and which
// none of this is about: what matters here is that the panel saves whatever
// markdown the editor hands it, and that an archived board can't type.
vi.mock('./MarkdownEditor', () => ({
  MarkdownEditor: ({
    initialValue,
    onChange,
    ariaLabel,
    placeholder,
    editable = true,
  }: {
    initialValue: string;
    onChange: (markdown: string) => void;
    ariaLabel: string;
    placeholder?: string;
    editable?: boolean;
  }) => (
    <textarea
      aria-label={ariaLabel}
      placeholder={placeholder}
      defaultValue={initialValue}
      readOnly={!editable}
      onChange={(event) => {
        onChange(event.target.value);
      }}
    />
  ),
}));

function project(overrides: Partial<Project> = {}): Project {
  return {
    id: '01JP',
    name: 'Kanban',
    description: 'the board',
    workdir: '/tmp/kanban',
    max_parallel_issue_runs: 3,
    agents_may_merge: false,
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
    const box = screen.getByLabelText('Daily budget (USD)');
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
    await userEvent.clear(screen.getByLabelText('Daily budget (USD)'));
    await userEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => {
      expect(api.updateProject).toHaveBeenCalledWith(
        client,
        '01JP',
        expect.objectContaining({ daily_budget_micros: null }),
      );
    });
  });

  it('shows a stored token ceiling and saves it back as a count', async () => {
    render(
      <ProjectSettings
        project={project({ daily_budget_tokens: 250_000 })}
        onClose={vi.fn()}
        onSaved={vi.fn()}
      />,
    );
    const box = screen.getByLabelText('Daily token budget (tokens)');
    expect(box).toHaveValue('250000');

    await userEvent.clear(box);
    await userEvent.type(box, '400000');
    await userEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => {
      expect(api.updateProject).toHaveBeenCalledWith(
        client,
        '01JP',
        expect.objectContaining({ daily_budget_tokens: 400_000 }),
      );
    });
  });

  it('sends null when the token box is emptied, so that ceiling can be removed too', async () => {
    render(
      <ProjectSettings
        project={project({ daily_budget_tokens: 250_000 })}
        onClose={vi.fn()}
        onSaved={vi.fn()}
      />,
    );
    await userEvent.clear(screen.getByLabelText('Daily token budget (tokens)'));
    await userEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => {
      expect(api.updateProject).toHaveBeenCalledWith(
        client,
        '01JP',
        expect.objectContaining({ daily_budget_tokens: null }),
      );
    });
  });

  it('refuses a token ceiling that is not a whole count, and saves nothing', async () => {
    render(
      <ProjectSettings
        project={project()}
        onClose={vi.fn()}
        onSaved={vi.fn()}
      />,
    );
    await userEvent.type(screen.getByLabelText('Daily token budget (tokens)'), '250k');
    await userEvent.click(screen.getByRole('button', { name: 'Save' }));
    expect(await screen.findByText(/whole number of tokens/)).toBeInTheDocument();
    expect(api.updateProject).not.toHaveBeenCalled();
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
    await userEvent.type(screen.getByLabelText('Daily budget (USD)'), 'lots');
    await userEvent.click(screen.getByRole('button', { name: 'Save' }));
    expect(await screen.findByText(/must be an amount in dollars/)).toBeInTheDocument();
    expect(api.updateProject).not.toHaveBeenCalled();
  });

  it('says which unit each ceiling is in', () => {
    // Two identical boxes whose units are 10^6 apart. Unlabelled, "5" in the
    // money box is $5 and "5" in the token box is five tokens, and nothing
    // on screen said which was which — the live board that started all this
    // carries a 100,000-token ceiling, a tenth of one run.
    render(
      <ProjectSettings
        project={project()}
        onClose={vi.fn()}
        onSaved={vi.fn()}
      />,
    );
    expect(screen.getByLabelText('Daily budget (USD)')).toHaveAttribute(
      'placeholder',
      expect.stringContaining('5.00'),
    );
    expect(screen.getByLabelText('Daily token budget (tokens)')).toHaveAttribute(
      'placeholder',
      expect.stringContaining('2000000'),
    );
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
    expect(screen.getByLabelText('Daily budget (USD)')).toBeDisabled();
    expect(screen.getByLabelText('Daily token budget (tokens)')).toBeDisabled();
    expect(screen.queryByRole('button', { name: 'Save' })).not.toBeInTheDocument();

    expect(screen.getByLabelText('Description')).toHaveAttribute('readonly');

    // Un-archiving is the way back and asks nothing: the confirmation guards
    // the direction that takes the board away, not the one that returns it.
    await userEvent.click(screen.getByRole('button', { name: 'Unarchive' }));
    expect(api.setProjectArchived).toHaveBeenCalledWith(client, '01JP', false);
  });

  it('asks before archiving, and archives only once the question is answered', async () => {
    render(<ProjectSettings project={project()} onClose={vi.fn()} onSaved={vi.fn()} />);

    await userEvent.click(screen.getByRole('button', { name: 'Archive' }));
    expect(api.setProjectArchived).not.toHaveBeenCalled();
    expect(screen.getByRole('dialog', { name: 'Archive project' })).toBeInTheDocument();

    await userEvent.click(screen.getByRole('button', { name: 'Archive it' }));
    await waitFor(() => {
      expect(api.setProjectArchived).toHaveBeenCalledWith(client, '01JP', true);
    });
  });

  it('leaves the board alone when the question is waved off', async () => {
    render(<ProjectSettings project={project()} onClose={vi.fn()} onSaved={vi.fn()} />);

    await userEvent.click(screen.getByRole('button', { name: 'Archive' }));
    await userEvent.click(screen.getByRole('button', { name: 'Never mind' }));

    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    expect(api.setProjectArchived).not.toHaveBeenCalled();
  });

  it('saves the description as the markdown the editor produced', async () => {
    render(<ProjectSettings project={project()} onClose={vi.fn()} onSaved={vi.fn()} />);

    const box = screen.getByLabelText('Description');
    expect(box).toHaveValue('the board');

    await userEvent.clear(box);
    await userEvent.type(box, '## Goals');
    await userEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => {
      expect(api.updateProject).toHaveBeenCalledWith(
        client,
        '01JP',
        expect.objectContaining({ description: '## Goals' }),
      );
    });
  });

  it('leaves the roster to the board, keeping one place a team is managed', () => {
    // The team strip above the board is the one. Settings carried a second
    // copy of it, so a hire had two front doors that had to agree.
    render(<ProjectSettings project={project()} onClose={vi.fn()} onSaved={vi.fn()} />);

    expect(screen.queryByText('Team')).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /New agent/ })).not.toBeInTheDocument();
  });

});
