import { beforeEach, describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { QueuePanel } from './QueuePanel';
import { QueueProvider, type PauseReason, type QueuedItem } from './queueStore';
import { installMemoryLocalStorage } from '../../test/memoryStorage';

// Renders <QueuePanel> over a real <QueueProvider>, seeded through
// localStorage, and drives it with user-event: row order, the per-row
// send/delete callbacks, inline edit (save on Enter, revert on Esc), and the
// pause banner + bulk resume. dnd-kit pointer drag does not work in jsdom, so
// reorder is covered by the store's unit test, not here. See
// docs/todo/web-unit-tests.md.

const SID = 'sess-1';
const KEY = `baybo.queue.${SID}`;
const qi = (id: string, text: string): QueuedItem => ({ id, text, attachments: [] });

beforeEach(() => {
  installMemoryLocalStorage();
});

function seed(items: QueuedItem[], pauseReason: PauseReason = null) {
  window.localStorage.setItem(KEY, JSON.stringify({ items, deferred: [], pauseReason }));
}

function renderPanel() {
  const onFire = vi.fn();
  const onResume = vi.fn();
  const utils = render(
    <QueueProvider>
      <QueuePanel sessionId={SID} baseUrl="" adminToken={null} onFire={onFire} onResume={onResume} />
    </QueueProvider>,
  );
  return { onFire, onResume, ...utils };
}

describe('QueuePanel', () => {
  it('renders nothing when the queue is empty', () => {
    const { container } = renderPanel();
    expect(container.firstChild).toBeNull();
  });

  it('renders parked messages in queue order', () => {
    seed([qi('a', 'first'), qi('b', 'second')]);
    renderPanel();
    const texts = screen.getAllByText(/^(first|second)$/).map((el) => el.textContent);
    expect(texts).toEqual(['first', 'second']);
  });

  it('the send button fires onFire with that item', async () => {
    seed([qi('a', 'ping')]);
    const user = userEvent.setup();
    const { onFire } = renderPanel();
    await user.click(screen.getByLabelText('Send this message now'));
    expect(onFire).toHaveBeenCalledTimes(1);
    expect(onFire.mock.calls[0][0]).toMatchObject({ id: 'a', text: 'ping' });
  });

  it('the delete button removes just that row', async () => {
    seed([qi('a', 'gone'), qi('b', 'stays')]);
    const user = userEvent.setup();
    renderPanel();
    await user.click(screen.getAllByLabelText('Remove from queue')[0]);
    expect(screen.queryByText('gone')).toBeNull();
    expect(screen.getByText('stays')).toBeInTheDocument();
  });

  it('inline edit saves the new text on Enter', async () => {
    seed([qi('a', 'typo')]);
    const user = userEvent.setup();
    renderPanel();
    await user.click(screen.getByLabelText('Edit message'));
    const box = screen.getByRole('textbox');
    await user.clear(box);
    await user.type(box, 'fixed{Enter}');
    expect(screen.queryByRole('textbox')).toBeNull();
    expect(screen.getByText('fixed')).toBeInTheDocument();
  });

  it('inline edit reverts on Esc', async () => {
    seed([qi('a', 'original')]);
    const user = userEvent.setup();
    renderPanel();
    await user.click(screen.getByLabelText('Edit message'));
    const box = screen.getByRole('textbox');
    await user.clear(box);
    await user.type(box, 'discard{Escape}');
    expect(screen.queryByRole('textbox')).toBeNull();
    expect(screen.getByText('original')).toBeInTheDocument();
    expect(screen.queryByText('discard')).toBeNull();
  });

  it('shows the cancelled banner and Send remaining calls onResume', async () => {
    seed([qi('a', 'held')], 'cancelled');
    const user = userEvent.setup();
    const { onResume } = renderPanel();
    expect(screen.getByText(/Turn cancelled/)).toBeInTheDocument();
    await user.click(screen.getByText('Send remaining'));
    expect(onResume).toHaveBeenCalledTimes(1);
  });

  it('the error banner reads differently', () => {
    seed([qi('a', 'held')], 'error');
    renderPanel();
    expect(screen.getByText(/Turn failed/)).toBeInTheDocument();
  });
});
