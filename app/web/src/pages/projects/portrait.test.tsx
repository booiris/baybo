import { afterEach, describe, expect, it, vi } from 'vitest';
import { renderHook, waitFor } from '@testing-library/react';

import { botttsFace } from '../../components/botttsFace';
import type { Agent } from './boardModel';
import { generatedPortrait, useTeamPortraits } from './portrait';

const auth = { baseUrl: 'http://board.test/', token: 'tok', logout: vi.fn() };
vi.mock('../../api/auth', () => ({
  useAuth: () => auth,
  useAdminClient: () => ({}),
}));

const DEV_1 = '01KZAD1QBS4A1XH456XJ7AC0V9';
const DEV_2 = '01KZ85BA2R7HZ6ZNY4N6RSV52N';

function member(id: string, handle: string, avatar?: string): Agent {
  return {
    id,
    handle,
    name: handle,
    description: '',
    framework: 'baybo',
    lead: false,
    created_at_ms: 0,
    ...(avatar === undefined ? {} : { avatar_blob_id: avatar }),
  };
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('generatedPortrait', () => {
  it('gives an agent its own face and the operator none', () => {
    expect(generatedPortrait(DEV_1)).toBe(botttsFace(DEV_1));
    expect(generatedPortrait(null)).toBeNull();
    expect(generatedPortrait(undefined)).toBeNull();
    expect(generatedPortrait('')).toBeNull();
  });
});

describe('useTeamPortraits', () => {
  it('lets an uploaded avatar beat the generated one, per agent', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, blob: () => Promise.resolve({}) }));
    vi.stubGlobal('URL', { ...URL, createObjectURL: () => 'blob:uploaded' });

    const team = [member(DEV_1, 'dev-1', 'sha256:aa.bb'), member(DEV_2, 'dev-2')];
    const { result } = renderHook(() => useTeamPortraits(team));

    await waitFor(() => {
      expect(result.current(DEV_1)).toBe('blob:uploaded');
    });
    // The teammate nobody uploaded a face for keeps its generated one rather
    // than going blank while the other is fetched.
    expect(result.current(DEV_2)).toBe(botttsFace(DEV_2));
  });

  it('falls back to the generated face when the blob cannot be fetched', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: false, status: 404 }));

    const team = [member(DEV_1, 'dev-1', 'sha256:gone.404')];
    const { result } = renderHook(() => useTeamPortraits(team));

    await waitFor(() => {
      expect(fetch).toHaveBeenCalled();
    });
    expect(result.current(DEV_1)).toBe(botttsFace(DEV_1));
  });

  it('fetches nothing at all for a team with no uploads', () => {
    const fetchSpy = vi.fn();
    vi.stubGlobal('fetch', fetchSpy);
    renderHook(() => useTeamPortraits([member(DEV_1, 'dev-1'), member(DEV_2, 'dev-2')]));
    expect(fetchSpy).not.toHaveBeenCalled();
  });
});
