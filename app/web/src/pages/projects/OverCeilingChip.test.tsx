import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { OverCeilingChip } from './OverCeilingChip';
import type { Project } from './boardModel';
import type { BoardActivity } from './useBoardActivity';

function project(overrides: Partial<Project> = {}): Project {
  return {
    id: '01JPROJECT',
    name: 'rglide',
    description: '',
    workdir: '/tmp/rglide',
    max_parallel_issue_runs: 3,
    created_at_ms: 0,
    updated_at_ms: 0,
    ...overrides,
  };
}

function burn(tokens: number, micros = 0): BoardActivity[] {
  return [{ project_id: '01JPROJECT', working: 0, burn_micros: micros, burn_tokens: tokens }];
}

describe('OverCeilingChip', () => {
  it('says nothing at all on a board with no ceiling', () => {
    const { container } = render(
      <OverCeilingChip project={project()} activity={burn(9_999_999)} held={0} />,
    );
    expect(container).toBeEmptyDOMElement();
  });

  it('says nothing while the board is merely close to its ceiling', () => {
    const { container } = render(
      <OverCeilingChip
        project={project({ daily_budget_tokens: 100_000 })}
        activity={burn(96_000)}
        held={0}
      />,
    );
    expect(container).toBeEmptyDOMElement();
  });

  it('names the ceiling, the spend and the runs it is holding', () => {
    // The live board this was written for: a 100k/day token ceiling on a
    // board whose median run costs ~1M, six times past it by 00:00:33 UTC,
    // two follow-up runs held — and nothing anywhere on the board saying so.
    render(
      <OverCeilingChip
        project={project({ daily_budget_tokens: 100_000 })}
        activity={burn(601_902)}
        held={2}
      />,
    );
    // The figure is on screen at rest — that is the whole point of a chip
    // over a dot. The sentence rides in the title, where a header control
    // this size has to keep it.
    const chip = screen.getByRole('status');
    expect(chip).toHaveTextContent('602k / 100k');
    expect(chip).toHaveAttribute(
      'title',
      expect.stringContaining('Over the daily ceiling'),
    );
    expect(chip).toHaveAttribute('title', expect.stringContaining('2 held on budget'));
  });

  it('does not claim to be holding runs when it is holding none', () => {
    // A ceiling of 0 is the pause, and it is over from the first second of
    // the day — before anything has been enqueued to hold.
    render(
      <OverCeilingChip
        project={project({ daily_budget_tokens: 0 })}
        activity={burn(0)}
        held={0}
      />,
    );
    const chip = screen.getByRole('status');
    expect(chip).toHaveAttribute('title', expect.stringContaining('Over the daily ceiling'));
    expect(chip).toHaveAttribute('title', expect.not.stringContaining('held on budget'));
  });

  it('names both ceilings when both are set, marking the one that bit', () => {
    // `boardMeter` picks one to speak in; a readout must not. Money and
    // tokens are independent gates and either stops the board, so an
    // operator shown only the tighter of two that are both spent raises one
    // and watches nothing happen.
    render(
      <OverCeilingChip
        project={project({ daily_budget_micros: 5_000_000, daily_budget_tokens: 100_000 })}
        activity={burn(601_902, 1_200_000)}
        held={2}
      />,
    );
    const chip = screen.getByRole('status');
    expect(chip).toHaveTextContent('$1.20 / $5.00');
    expect(chip).toHaveTextContent('602k / 100k');
    // The money ceiling has room left, so it is dimmed; the token one bit.
    expect(screen.getByText('$1.20 / $5.00')).toHaveClass('opacity-55');
    expect(screen.getByText('602k / 100k')).not.toHaveClass('opacity-55');
    expect(chip).toHaveAttribute(
      'title',
      expect.stringContaining('$1.20 / $5.00 and 602k / 100k spent today'),
    );
  });

  it('is the press that opens the setting, and a plain chip where there is none', async () => {
    const onOpenSettings = vi.fn();
    const { rerender } = render(
      <OverCeilingChip
        project={project({ daily_budget_tokens: 100_000 })}
        activity={burn(601_902)}
        held={2}
        onOpenSettings={onOpenSettings}
      />,
    );
    await userEvent.click(screen.getByRole('button'));
    expect(onOpenSettings).toHaveBeenCalledOnce();

    // The column page has no settings modal of its own; the notice is still
    // the answer to "why is nothing moving" there.
    rerender(
      <OverCeilingChip
        project={project({ daily_budget_tokens: 100_000 })}
        activity={burn(601_902)}
        held={2}
      />,
    );
    expect(screen.queryByRole('button')).toBeNull();
    expect(screen.getByRole('status')).toHaveTextContent('602k / 100k');
  });
});
