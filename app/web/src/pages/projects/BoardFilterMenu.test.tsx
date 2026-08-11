import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { BoardFilterMenu } from './BoardFilterMenu';
import { EMPTY_FILTER, type BoardFilter } from './boardFilter';

async function openMenu(filter: BoardFilter = EMPTY_FILTER) {
  const onChange = vi.fn();
  const view = render(<BoardFilterMenu filter={filter} team={[]} onChange={onChange} />);
  await userEvent.click(screen.getByLabelText(/^Filter the board/));
  const field = screen.getByLabelText('Filter cards by title or number');
  /// Re-render with whatever the URL now says. Passing the same filter is what
  /// the board really does mid-composition: nothing has been published yet, so
  /// there is nothing new for it to parse back out.
  const restate = (next: BoardFilter = filter) => {
    view.rerender(<BoardFilterMenu filter={next} team={[]} onChange={onChange} />);
  };
  return { onChange, field, restate };
}

describe('BoardFilterMenu search', () => {
  it('publishes every keystroke when no IME is involved', async () => {
    const { onChange, field } = await openMenu();
    fireEvent.change(field, { target: { value: 'parser' } });
    expect(onChange).toHaveBeenCalledWith({ ...EMPTY_FILTER, text: 'parser' });
  });

  it('says nothing while a candidate is still being composed', async () => {
    const { onChange, field } = await openMenu();
    fireEvent.compositionStart(field);
    fireEvent.change(field, { target: { value: 'kan' } });
    fireEvent.change(field, { target: { value: 'kanban' } });

    // Half-composed pinyin is not a search. Publishing it would filter the
    // board down to nothing on every keystroke of a Chinese word.
    expect(onChange).not.toHaveBeenCalled();
    expect(field).toHaveValue('kanban');
  });

  it('keeps the candidate when the board re-renders mid-composition', async () => {
    const { field, restate } = await openMenu();
    fireEvent.compositionStart(field);
    fireEvent.change(field, { target: { value: 'kanban' } });

    // The bug this guards: the value used to come straight back from the URL,
    // so a render landing here wrote the stale text over the live candidate
    // and the IME lost it.
    restate();
    expect(field).toHaveValue('kanban');
  });

  it('publishes once the candidate is committed', async () => {
    const { onChange, field } = await openMenu();
    fireEvent.compositionStart(field);
    fireEvent.change(field, { target: { value: 'kanban' } });
    fireEvent.compositionEnd(field, { target: { value: '看板' } });

    expect(onChange).toHaveBeenCalledTimes(1);
    expect(onChange).toHaveBeenCalledWith({ ...EMPTY_FILTER, text: '看板' });
    expect(field).toHaveValue('看板');
  });

  it('still follows the filter when it changes from outside the field', async () => {
    const { field, restate } = await openMenu({ ...EMPTY_FILTER, text: 'parser' });
    expect(field).toHaveValue('parser');
    // Clear, a pasted link, the back button. A draft that stopped listening to
    // the URL would strand the old text in the box with the board unfiltered.
    restate(EMPTY_FILTER);
    expect(field).toHaveValue('');
  });
});
