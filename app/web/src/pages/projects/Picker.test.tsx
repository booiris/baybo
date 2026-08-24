import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { Picker } from './Picker';

const OPTIONS = [
  { value: 'todo', label: 'Todo' },
  { value: 'in_progress', label: 'In Progress' },
  { value: 'done', label: 'Done' },
];

function renderPicker(disabled = false) {
  const onPick = vi.fn();
  render(
    <div>
      <button type="button">outside</button>
      <Picker
        label="Status"
        value="todo"
        disabled={disabled}
        options={OPTIONS}
        onPick={onPick}
      >
        Todo
      </Picker>
    </div>,
  );
  return onPick;
}

describe('Picker', () => {
  it('is a button that names what it is set to, not a hidden form control', () => {
    renderPicker();
    const trigger = screen.getByLabelText('Status: Todo');
    expect(trigger.tagName).toBe('BUTTON');
    // The old picker was a transparent <select>: a control the eye could not
    // find and the OS drew its own dropdown for.
    expect(document.querySelector('select')).toBeNull();
    expect(trigger).toHaveAttribute('aria-expanded', 'false');
  });

  it('opens the choices and reports the one that was pressed', async () => {
    const onPick = renderPicker();
    await userEvent.click(screen.getByLabelText('Status: Todo'));
    expect(screen.getByLabelText('Status: Todo')).toHaveAttribute('aria-expanded', 'true');

    await userEvent.click(screen.getByRole('button', { name: 'Done' }));
    expect(onPick).toHaveBeenCalledWith('done');
    expect(screen.queryByRole('button', { name: 'Done' })).toBeNull();
  });

  it('says nothing when the choice made is the one already set', async () => {
    const onPick = renderPicker();
    await userEvent.click(screen.getByLabelText('Status: Todo'));
    // The trigger is named "Status: Todo", so this only matches the option.
    await userEvent.click(screen.getByRole('button', { name: 'Todo' }));
    // Picking what is already picked is a dismissal, not a move — and a move
    // here refetches the board and rewrites the whole column's order.
    expect(onPick).not.toHaveBeenCalled();
  });

  it('closes on a click elsewhere and on Escape', async () => {
    renderPicker();
    await userEvent.click(screen.getByLabelText('Status: Todo'));
    await userEvent.click(screen.getByRole('button', { name: 'outside' }));
    expect(screen.queryByRole('button', { name: 'In Progress' })).toBeNull();

    await userEvent.click(screen.getByLabelText('Status: Todo'));
    await userEvent.keyboard('{Escape}');
    expect(screen.queryByRole('button', { name: 'In Progress' })).toBeNull();
    // Escape hands the focus back rather than dropping it on the body.
    expect(screen.getByLabelText('Status: Todo')).toHaveFocus();
  });

  it('does not open at all when it is disabled', async () => {
    renderPicker(true);
    await userEvent.click(screen.getByLabelText('Status: Todo'));
    expect(screen.queryByRole('button', { name: 'In Progress' })).toBeNull();
  });
});
