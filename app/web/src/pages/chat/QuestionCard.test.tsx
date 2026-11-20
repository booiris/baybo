import { useState } from 'react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/react';

import type { WireQuestion, WireQuestionReply } from '../../api/chatWs';
import { QuestionCard } from './QuestionCard';

const TWO_QUESTIONS: WireQuestion[] = [
  {
    header: 'Auth',
    question: 'Which auth strategy should we use?',
    options: [
      { label: 'JWT', description: 'Stateless bearer tokens' },
      { label: 'Session', description: 'Cookie-backed server sessions' },
    ],
    multi_select: false,
  },
  {
    header: 'DB',
    question: 'Which data stores should we enable?',
    options: [
      { label: 'Postgres', description: 'Primary relational store' },
      { label: 'Redis', description: 'Cache and pub/sub' },
      { label: 'SQLite', description: 'Embedded local db' },
    ],
    multi_select: true,
  },
];

function StatefulCard(props: {
  questions?: WireQuestion[];
  onSubmit: (reply: WireQuestionReply) => boolean;
  connected?: boolean;
  queuedCount?: number;
}) {
  const [tab, setTab] = useState(0);
  return (
    <QuestionCard
      askId="ask-1234567890"
      questions={props.questions ?? TWO_QUESTIONS}
      activeTab={tab}
      onTabChange={setTab}
      onSubmit={props.onSubmit}
      connected={props.connected ?? true}
      queuedCount={props.queuedCount}
    />
  );
}

describe('QuestionCard', () => {
  it('enables Submit only once every tab has a selection or Other text', () => {
    const onSubmit = vi.fn(() => true);
    render(<StatefulCard onSubmit={onSubmit} />);

    const submit = screen.getByRole('button', { name: /submit/i });
    expect(submit).toBeDisabled();

    // Answer tab 1 (single-select radio)
    fireEvent.click(screen.getByLabelText(/JWT/));
    expect(submit).toBeDisabled();

    expect(screen.getByRole('tab', { name: /DB/ })).toHaveFocus();
    fireEvent.click(screen.getByLabelText(/Postgres/));
    fireEvent.click(screen.getByLabelText(/Redis/));
    fireEvent.change(screen.getByPlaceholderText('Other…'), { target: { value: '  DuckDB  ' } });

    expect(submit).not.toBeDisabled();
    fireEvent.click(submit);

    expect(onSubmit).toHaveBeenCalledTimes(1);
    expect(onSubmit).toHaveBeenCalledWith({
      kind: 'answered',
      answers: [
        { header: 'Auth', selected: ['JWT'] },
        { header: 'DB', selected: ['Postgres', 'Redis'], other: 'DuckDB' },
      ],
    });
  });

  it('keeps the choices available for retry if the socket closes before send', () => {
    const onSubmit = vi.fn(() => true).mockReturnValueOnce(false);
    render(<StatefulCard questions={[TWO_QUESTIONS[0]]} onSubmit={onSubmit} />);
    fireEvent.click(screen.getByLabelText(/JWT/));
    const submit = screen.getByRole('button', { name: /submit/i });
    fireEvent.click(submit);
    expect(submit).toBeEnabled();
    fireEvent.click(submit);
    expect(submit).toBeDisabled();
    expect(onSubmit.mock.calls[0]).toEqual(onSubmit.mock.calls[1]);
  });

  it('single-select radio and Other text are mutually exclusive', () => {
    const onSubmit = vi.fn(() => true);
    render(<StatefulCard questions={[TWO_QUESTIONS[0]]} onSubmit={onSubmit} />);

    fireEvent.click(screen.getByLabelText(/JWT/));
    const other = screen.getByPlaceholderText('Other…') as HTMLTextAreaElement;
    fireEvent.change(other, { target: { value: 'mTLS' } });
    expect((screen.getByLabelText(/JWT/) as HTMLInputElement).checked).toBe(false);

    // Picking an option again clears Other
    fireEvent.click(screen.getByLabelText(/Session/));
    expect(other.value).toBe('');
    expect((screen.getByLabelText(/Session/) as HTMLInputElement).checked).toBe(true);
  });

  it('requires a second click on the same button to cancel the ask', async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn(() => true);
    render(<StatefulCard onSubmit={onSubmit} />);

    await user.click(screen.getByLabelText(/JWT/));
    const button = screen.getByRole('button', { name: /cancel ask/i });
    await user.click(button);
    expect(button).toHaveAccessibleName('Confirm cancel?');
    expect(onSubmit).not.toHaveBeenCalled();
    await user.click(button);
    expect(button).toBeDisabled();
    expect(onSubmit).toHaveBeenCalledTimes(1);
    expect(onSubmit).toHaveBeenCalledWith({ kind: 'cancelled' });
  });

  it('dismisses cancel confirmation on Escape or when returning to the answer', async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn(() => true);
    render(<StatefulCard onSubmit={onSubmit} />);
    const button = screen.getByRole('button', { name: 'Cancel ask' });
    await user.click(button);
    await user.keyboard('{Escape}');
    expect(button).toHaveAccessibleName('Cancel ask');
    await user.keyboard('{Enter}');
    expect(button).toHaveAccessibleName('Confirm cancel?');
    await user.click(screen.getByRole('textbox'));
    expect(button).toHaveAccessibleName('Cancel ask');
    await user.click(button);
    expect(button).toHaveAccessibleName('Confirm cancel?');
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it('uses arrows and numbers outside text fields and submits only on explicit activation', () => {
    const onSubmit = vi.fn(() => true);
    render(<StatefulCard onSubmit={onSubmit} />);

    // While Other input is focused, pressing 1 or ArrowRight does nothing.
    const other = screen.getByPlaceholderText('Other…');
    other.focus();
    fireEvent.keyDown(other, { key: '1' });
    fireEvent.keyDown(other, { key: 'ArrowRight' });
    expect(screen.getByRole('tab', { name: /Auth/ })).toHaveAttribute('aria-selected', 'true');
    expect((screen.getByLabelText(/JWT/) as HTMLInputElement).checked).toBe(false);

    // The number shortcut activates the same option as a click.
    other.blur();
    fireEvent.keyDown(document.body, { key: '1' });
    expect(screen.getByRole('tab', { name: /DB/ })).toHaveFocus();
    fireEvent.keyDown(document.body, { key: 'ArrowLeft' });
    expect(screen.getByLabelText(/JWT/)).toBeChecked();
    fireEvent.keyDown(document.body, { key: 'ArrowRight' });
    expect(screen.getByRole('tab', { name: /DB/ })).toHaveAttribute('aria-selected', 'true');
    fireEvent.keyDown(document.body, { key: '2' });
    fireEvent.keyDown(document.body, { key: 'Enter' });
    expect(onSubmit).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: /submit/i }));

    expect(onSubmit).toHaveBeenCalledTimes(1);
    expect(onSubmit).toHaveBeenCalledWith({
      kind: 'answered',
      answers: [
        { header: 'Auth', selected: ['JWT'] },
        { header: 'DB', selected: ['Redis'] },
      ],
    });
  });

  it('stays on multi-select and final tabs, and advances when re-clicking a selected radio', async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn(() => true);
    render(<StatefulCard questions={[...TWO_QUESTIONS, { ...TWO_QUESTIONS[0], header: 'Backup' }]} onSubmit={onSubmit} />);
    await user.click(screen.getByText('JWT'));
    await user.click(screen.getByRole('tab', { name: /Auth/ }));
    await user.click(screen.getByRole('radio', { name: /JWT/ }));
    await user.click(screen.getByLabelText(/Redis/));
    expect(screen.getByRole('tab', { name: /DB/ })).toHaveAttribute('aria-selected', 'true');
    await user.click(screen.getByRole('tab', { name: /Backup/ }));
    await user.click(screen.getByLabelText(/Session/));
    expect(screen.getByRole('tab', { name: /Backup/ })).toHaveAttribute('aria-selected', 'true');
    expect(screen.getByRole('button', { name: /submit/i })).toBeEnabled();
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it('inserts a newline in Other and includes both lines in the submitted answer', async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn(() => true);
    render(<StatefulCard questions={[TWO_QUESTIONS[0]]} onSubmit={onSubmit} />);
    await user.type(screen.getByRole('textbox'), 'first{Enter}second');
    expect(onSubmit).not.toHaveBeenCalled();
    await user.click(screen.getByRole('button', { name: /submit/i }));
    expect(onSubmit).toHaveBeenCalledWith({
      kind: 'answered',
      answers: [{ header: 'Auth', selected: [], other: 'first\nsecond' }],
    });
  });

  it('lets Tab visit each question and then reach the answer controls', async () => {
    const user = userEvent.setup();
    render(<StatefulCard onSubmit={() => true} />);
    await user.tab();
    expect(screen.getByRole('tab', { name: /Auth/ })).toHaveFocus();
    await user.tab();
    expect(screen.getByRole('tab', { name: /DB/ })).toHaveAttribute('aria-selected', 'true');
    await user.tab();
    expect(screen.getByLabelText(/Postgres/)).toHaveFocus();
  });

  it('shows +N waiting badge when queued asks exist and disables buttons when disconnected', () => {
    const onSubmit = vi.fn(() => true);
    render(<StatefulCard onSubmit={onSubmit} connected={false} queuedCount={2} />);

    expect(screen.getByText('+2 waiting')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /submit/i })).toBeDisabled();
    expect(screen.getByRole('button', { name: /cancel ask/i })).toBeDisabled();
    expect(screen.getByText(/waiting for connection/i)).toBeInTheDocument();
  });
});
