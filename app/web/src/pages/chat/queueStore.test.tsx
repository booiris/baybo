import { beforeEach, describe, expect, it } from 'vitest';
import { act, renderHook } from '@testing-library/react';

import { QueueProvider, useQueueStore, useSessionQueue, type QueuedItem } from './queueStore';
import { installMemoryLocalStorage } from '../../test/memoryStorage';

// Renders the reactive store hook (`useSessionQueue`) inside a real
// <QueueProvider> and drives it through `act()`, pinning the invariants the
// pure-function suites can't reach: the synchronous `queuesRef` composition
// (resume = clearPause then popTop in one tick), the empty-queue pause
// collapse (`normalize`), FIFO order, localStorage persistence, and the
// cross-tab `storage` listener. See docs/web-unit-tests.md.

const SID = 'sess-1';
const KEY = `baybo.queue.${SID}`;
const qi = (id: string, text: string): QueuedItem => ({ id, text, attachments: [] });

beforeEach(() => {
  installMemoryLocalStorage();
});

function mountQueue(sid = SID) {
  return renderHook(() => useSessionQueue(sid), { wrapper: QueueProvider });
}

describe('queueStore — parked items', () => {
  it('appends FIFO and reflects the change reactively', () => {
    const { result } = mountQueue();
    act(() => {
      result.current.enqueue(qi('a', 'first'));
      result.current.enqueue(qi('b', 'second'));
    });
    expect(result.current.items.map((i) => i.text)).toEqual(['first', 'second']);
  });

  it('popTop returns and removes the head', () => {
    const { result } = mountQueue();
    act(() => {
      result.current.enqueue(qi('a', 'first'));
      result.current.enqueue(qi('b', 'second'));
    });
    let popped: QueuedItem | undefined;
    act(() => {
      popped = result.current.popTop();
    });
    expect(popped?.id).toBe('a');
    expect(result.current.items.map((i) => i.id)).toEqual(['b']);
  });

  it('reorder rearranges by id and drops ids no longer present', () => {
    const { result } = mountQueue();
    act(() => {
      result.current.enqueue(qi('a', 'A'));
      result.current.enqueue(qi('b', 'B'));
      result.current.enqueue(qi('c', 'C'));
    });
    act(() => {
      result.current.reorder(['c', 'a', 'b', 'ghost']);
    });
    expect(result.current.items.map((i) => i.id)).toEqual(['c', 'a', 'b']);
  });

  it('editItem replaces the text in place', () => {
    const { result } = mountQueue();
    act(() => {
      result.current.enqueue(qi('a', 'old'));
    });
    act(() => {
      result.current.editItem('a', 'new');
    });
    expect(result.current.items[0].text).toBe('new');
  });
});

describe('queueStore — pause / resume', () => {
  it('collapses a pause once the queue drains empty (normalize)', () => {
    const { result } = mountQueue();
    act(() => {
      result.current.enqueue(qi('a', 'x'));
    });
    act(() => {
      result.current.setPause('cancelled');
    });
    expect(result.current.pauseReason).toBe('cancelled');
    act(() => {
      result.current.removeItem('a');
    });
    expect(result.current.items).toHaveLength(0);
    expect(result.current.pauseReason).toBeNull();
  });

  it('resume composes clearPause then popTop in a single tick', () => {
    const { result } = mountQueue();
    act(() => {
      result.current.enqueue(qi('a', 'first'));
      result.current.enqueue(qi('b', 'second'));
    });
    act(() => {
      result.current.setPause('cancelled');
    });
    let popped: QueuedItem | undefined;
    act(() => {
      result.current.clearPause();
      popped = result.current.popTop();
    });
    expect(popped?.id).toBe('a');
    expect(result.current.pauseReason).toBeNull();
    expect(result.current.items.map((i) => i.id)).toEqual(['b']);
  });
});

describe('queueStore — deferred plane', () => {
  it('deferItem parks a message behind the reply; restoreDeferred returns them to the front', () => {
    // restoreDeferred lives on the imperative store (the drain path calls it),
    // not the session-bound reactive hook — render both to drive and observe.
    const { result } = renderHook(() => ({ q: useSessionQueue(SID), api: useQueueStore() }), {
      wrapper: QueueProvider,
    });
    act(() => {
      result.current.q.enqueue(qi('a', 'A'));
      result.current.q.enqueue(qi('b', 'B'));
    });
    act(() => {
      result.current.q.deferItem('a');
    });
    expect(result.current.q.items.map((i) => i.id)).toEqual(['b']);
    expect(result.current.q.deferred.map((i) => i.id)).toEqual(['a']);
    act(() => {
      result.current.api.restoreDeferred(SID);
    });
    expect(result.current.q.deferred).toHaveLength(0);
    expect(result.current.q.items.map((i) => i.id)).toEqual(['a', 'b']);
  });
});

describe('queueStore — persistence', () => {
  it('persists to localStorage and a fresh provider reloads it', () => {
    const first = mountQueue();
    act(() => {
      first.result.current.enqueue(qi('a', 'kept'));
    });
    expect(window.localStorage.getItem(KEY)).not.toBeNull();
    first.unmount();

    const second = mountQueue();
    expect(second.result.current.items.map((i) => i.text)).toEqual(['kept']);
  });

  it("ingests another tab's write via the storage event", () => {
    const { result } = mountQueue();
    const payload = JSON.stringify({ items: [qi('x', 'from other tab')], deferred: [], pauseReason: null });
    act(() => {
      window.localStorage.setItem(KEY, payload);
      window.dispatchEvent(new StorageEvent('storage', { key: KEY, newValue: payload }));
    });
    expect(result.current.items.map((i) => i.text)).toEqual(['from other tab']);
  });
});
