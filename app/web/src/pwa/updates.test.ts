import { describe, expect, it, vi } from 'vitest';
import { SKIP_WAITING, ServiceWorkerUpdates, type WaitingWorker } from './updates';

function fakeWorker(): WaitingWorker & { messages: { type: string }[] } {
  const messages: { type: string }[] = [];
  return {
    messages,
    postMessage(message) {
      messages.push(message);
    },
  };
}

describe('ServiceWorkerUpdates', () => {
  it('says nothing on a first install', () => {
    const reload = vi.fn();
    const updates = new ServiceWorkerUpdates(reload);
    const notify = vi.fn();
    updates.subscribe(notify);

    updates.offer(fakeWorker(), false);

    expect(updates.getSnapshot().updateReady).toBe(false);
    expect(notify).not.toHaveBeenCalled();
  });

  it('prompts once when a newer worker installs behind a controlled page', () => {
    const updates = new ServiceWorkerUpdates(vi.fn());
    const notify = vi.fn();
    updates.subscribe(notify);

    updates.offer(fakeWorker(), true);
    const first = updates.getSnapshot();
    updates.offer(fakeWorker(), true);

    expect(first.updateReady).toBe(true);
    // The snapshot must keep its identity, or `useSyncExternalStore` re-renders
    // on every repeat offer.
    expect(updates.getSnapshot()).toBe(first);
    expect(notify).toHaveBeenCalledTimes(1);
  });

  it('hands over to the newest waiting worker', () => {
    const updates = new ServiceWorkerUpdates(vi.fn());
    const stale = fakeWorker();
    const fresh = fakeWorker();

    updates.offer(stale, true);
    updates.offer(fresh, true);
    updates.apply();

    expect(stale.messages).toEqual([]);
    expect(fresh.messages).toEqual([{ type: SKIP_WAITING }]);
  });

  it('ignores apply() with nothing waiting', () => {
    const reload = vi.fn();
    const updates = new ServiceWorkerUpdates(reload);

    updates.apply();
    updates.onControllerChange();

    expect(reload).not.toHaveBeenCalled();
  });

  it('does not reload when a first worker claims the page', () => {
    const reload = vi.fn();
    const updates = new ServiceWorkerUpdates(reload);

    // No apply(): this is `clients.claim()` on a first install, and reloading
    // here would flash the page every new visitor gets.
    updates.onControllerChange();

    expect(reload).not.toHaveBeenCalled();
  });

  it('reloads exactly once for an accepted update', () => {
    const reload = vi.fn();
    const updates = new ServiceWorkerUpdates(reload);
    updates.offer(fakeWorker(), true);

    updates.apply();
    updates.onControllerChange();
    updates.onControllerChange();

    expect(reload).toHaveBeenCalledTimes(1);
  });

  it('stops notifying an unsubscribed listener', () => {
    const updates = new ServiceWorkerUpdates(vi.fn());
    const notify = vi.fn();
    const unsubscribe = updates.subscribe(notify);

    unsubscribe();
    updates.offer(fakeWorker(), true);

    expect(notify).not.toHaveBeenCalled();
  });
});
