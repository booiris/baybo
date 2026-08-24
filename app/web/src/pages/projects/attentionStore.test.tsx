import { describe, expect, it, vi } from 'vitest';
import { act, renderHook } from '@testing-library/react';

import type { AdminClient } from '../../api/client';

/// The store is module state, so each test takes its own copy of the module.
/// Without this the first test's cached boards and live timer would be the
/// second test's starting position.
///
/// `useAdminClient` is mocked to hand back whatever this file most recently
/// set, so a test can swap gateways under a mounted hook the way a logout
/// and a fresh login do.
let currentClient: AdminClient;

vi.mock('../../api/auth', () => ({
  useAdminClient: () => currentClient,
}));

async function freshModule() {
  vi.resetModules();
  return import('./useAttention');
}

/// A client whose GET can be held open, so a test decides what is on the
/// wire when an invalidation arrives.
function fakeGateway(pages: { items: unknown[] }[]) {
  const pending: (() => void)[] = [];
  let call = 0;
  const GET = vi.fn(async () => {
    const data = pages[Math.min(call, pages.length - 1)];
    call += 1;
    await new Promise<void>((resolve) => pending.push(resolve));
    return { data, error: undefined, response: { ok: true } as Response };
  });
  return {
    client: { GET } as unknown as AdminClient,
    calls: () => GET.mock.calls.length,
    /// Let the oldest held request answer, and drain the microtasks its
    /// resolution schedules.
    answer: async () => {
      const next = pending.shift();
      expect(next).toBeDefined();
      await act(async () => {
        next?.();
        await Promise.resolve();
        await Promise.resolve();
        await Promise.resolve();
      });
    },
  };
}

const board = (name: string) => ({
  project_id: `01J${name}`,
  name,
  approvals: 1,
  failed: 0,
  unread: 0,
});

const LIT = { items: [board('Alpha')] };
const QUIET = { items: [] };

describe('the attention store', () => {
  it('coalesces a burst into one request', async () => {
    const { invalidateAttention } = await freshModule();
    const gateway = fakeGateway([LIT]);

    invalidateAttention(gateway.client);
    invalidateAttention(gateway.client);
    invalidateAttention(gateway.client);

    expect(gateway.calls()).toBe(1);
  });

  it('asks again for an invalidation raised while a poll was already open', async () => {
    // The whole point. The answer on the wire was computed before the act
    // that raised the second invalidation — the operator's approval, their
    // read — so discharging it with that answer leaves exactly the stale dot
    // this module exists to remove.
    const { invalidateAttention } = await freshModule();
    const gateway = fakeGateway([LIT, QUIET]);

    invalidateAttention(gateway.client);
    expect(gateway.calls()).toBe(1);

    invalidateAttention(gateway.client);
    expect(gateway.calls()).toBe(1);

    await gateway.answer();
    expect(gateway.calls()).toBe(2);
  });

  it('follows up exactly once however many invalidations piled up', async () => {
    const { invalidateAttention } = await freshModule();
    const gateway = fakeGateway([LIT, QUIET]);

    invalidateAttention(gateway.client);
    for (let i = 0; i < 5; i += 1) invalidateAttention(gateway.client);

    await gateway.answer();
    expect(gateway.calls()).toBe(2);

    await gateway.answer();
    expect(gateway.calls()).toBe(2);
  });

  it('gives up on a request that never answers, instead of wedging on it', async () => {
    // `fetch` has no timeout, and this store keeps exactly one request in
    // flight for every caller to join. A socket that opens and goes quiet
    // therefore used to pin `inFlight` forever: the badge froze at whatever
    // it last painted, for the life of the tab, with the server perfectly
    // healthy. That is this module's own failure mode — the un-clearable
    // dot it was written to prevent, arriving from the client side.
    vi.useFakeTimers();
    try {
      const { invalidateAttention } = await freshModule();
      const gateway = fakeGateway([LIT, QUIET]);

      invalidateAttention(gateway.client);
      expect(gateway.calls()).toBe(1);
      // Nothing answers. A second act finds the dead request and parks
      // behind it rather than asking.
      invalidateAttention(gateway.client);
      expect(gateway.calls()).toBe(1);

      await act(async () => {
        await vi.advanceTimersByTimeAsync(20_000);
      });

      // The abandoned poll released the store, and the invalidation it was
      // sitting on got a request of its own.
      expect(gateway.calls()).toBe(2);
    } finally {
      vi.useRealTimers();
    }
  });

  it('shares one cache and one request across every component', async () => {
    const { useAttention } = await freshModule();
    const gateway = fakeGateway([LIT]);
    currentClient = gateway.client;

    const rail = renderHook(() => useAttention());
    const switcher = renderHook(() => useAttention());
    expect(gateway.calls()).toBe(1);

    await gateway.answer();
    expect(rail.result.current).toHaveLength(1);
    expect(switcher.result.current).toHaveLength(1);
  });

  it('never paints another gateway’s boards', async () => {
    // Module state outlives a logout, and the hook seeds its first render
    // from the cache — so a second gateway must inherit neither the first
    // one's counts nor its in-flight response.
    const { useAttention } = await freshModule();
    const first = fakeGateway([LIT]);
    currentClient = first.client;

    const rail = renderHook(() => useAttention());
    await first.answer();
    expect(rail.result.current.map((b) => b.name)).toEqual(['Alpha']);
    rail.unmount();

    const second = fakeGateway([QUIET]);
    currentClient = second.client;
    const relogged = renderHook(() => useAttention());

    expect(relogged.result.current).toEqual([]);
    expect(second.calls()).toBe(1);
  });
});
