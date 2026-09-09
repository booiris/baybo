import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, renderHook } from '@testing-library/react';

import { DraftProvider, useDraftApi, useDrafts, type PendingAttachment } from './draftStore';
import { installMemoryLocalStorage } from '../../test/memoryStorage';

// The wiring a pure test over the map algebra cannot see: that mutations
// actually reach `localStorage`, that they are debounced rather than written per
// keystroke, that a flush happens on the way out, and that a fresh provider
// reads back what the last one wrote. Modelled on `queueStore.test.tsx`, the
// house's other per-session store test. See docs/web-unit-tests.md.

const KEY = 'baybo.draft.sess-1';

function mount() {
  return renderHook(() => ({ drafts: useDrafts(), api: useDraftApi() }), {
    wrapper: DraftProvider,
  });
}

const stored = (key = KEY): { text: string; attachments: unknown[] } | null => {
  const raw = window.localStorage.getItem(key);
  return raw === null ? null : (JSON.parse(raw) as { text: string; attachments: unknown[] });
};

beforeEach(() => {
  installMemoryLocalStorage();
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
  // The hidden-tab test overrides this; leaving it in place would run every
  // later test in the file against a permanently-backgrounded document.
  Object.defineProperty(document, 'visibilityState', {
    configurable: true,
    get: () => 'visible',
  });
});

describe('DraftProvider — reactivity', () => {
  it('holds one draft per conversation and never crosses them', () => {
    const { result } = mount();
    act(() => {
      result.current.api.setText('sess-1', 'for one');
      result.current.api.setText('sess-2', 'for two');
    });
    expect(result.current.drafts['sess-1']?.text).toBe('for one');
    expect(result.current.drafts['sess-2']?.text).toBe('for two');
  });

  it('composes two mutations in the same tick off the first', () => {
    const { result } = mount();
    act(() => {
      // The ref is updated synchronously ahead of the render, so the staged pick
      // is not clobbered by the text write landing in the same batch.
      result.current.api.setText('sess-1', 'look at this');
      result.current.api.stage('sess-1', {
        localId: 'p1',
        filename: 'shot.png',
        mime: 'image/png',
        size: 4,
        status: 'uploading',
      } satisfies PendingAttachment);
    });
    expect(result.current.drafts['sess-1']?.text).toBe('look at this');
    expect(result.current.drafts['sess-1']?.attachments).toHaveLength(1);
  });
});

describe('DraftProvider — persistence', () => {
  it('writes the draft once the debounce elapses, not per keystroke', () => {
    const { result } = mount();
    act(() => {
      result.current.api.setText('sess-1', 'h');
      result.current.api.setText('sess-1', 'he');
      result.current.api.setText('sess-1', 'hey');
    });
    expect(stored()).toBeNull();
    act(() => {
      vi.advanceTimersByTime(400);
    });
    expect(stored()?.text).toBe('hey');
  });

  it('a fresh provider reads back what the last one wrote', () => {
    const first = mount();
    act(() => {
      first.result.current.api.setText('sess-1', 'still here');
      vi.advanceTimersByTime(400);
    });
    first.unmount();

    const second = mount();
    expect(second.result.current.drafts['sess-1']?.text).toBe('still here');
  });

  it('flushes on unmount rather than losing the tail of what was typed', () => {
    const { result, unmount } = mount();
    act(() => {
      result.current.api.setText('sess-1', 'unflushed');
    });
    expect(stored()).toBeNull();
    unmount();
    expect(stored()?.text).toBe('unflushed');
  });

  it('flushes when the tab is hidden — the last callback before a discard', () => {
    const { result } = mount();
    act(() => {
      result.current.api.setText('sess-1', 'backgrounded');
    });
    act(() => {
      Object.defineProperty(document, 'visibilityState', {
        configurable: true,
        get: () => 'hidden',
      });
      document.dispatchEvent(new Event('visibilitychange'));
    });
    expect(stored()?.text).toBe('backgrounded');
  });

  it('persists a pick that reached the blob store, and no other kind', () => {
    const { result } = mount();
    act(() => {
      result.current.api.stage('sess-1', {
        localId: 'p1',
        filename: 'done.png',
        mime: 'image/png',
        size: 4,
        status: 'uploading',
      });
      result.current.api.stage('sess-1', {
        localId: 'p2',
        filename: 'inflight.png',
        mime: 'image/png',
        size: 4,
        status: 'uploading',
      });
      result.current.api.settle('sess-1', 'p1', { status: 'ready', blobId: 'blob-1' });
      vi.advanceTimersByTime(400);
    });
    // `p2` has no blob, so its bytes live only in this document — restoring it
    // as a chip that can never finish would wedge the send gate.
    expect(stored()?.attachments).toEqual([
      { localId: 'p1', filename: 'done.png', mime: 'image/png', size: 4, blobId: 'blob-1' },
    ]);
  });

  it('removes the row when the draft is sent, rather than leaving a husk', () => {
    const { result } = mount();
    act(() => {
      result.current.api.setText('sess-1', 'about to send');
      vi.advanceTimersByTime(400);
    });
    expect(stored()).not.toBeNull();
    act(() => {
      result.current.api.discard('sess-1');
      vi.advanceTimersByTime(400);
    });
    expect(window.localStorage.getItem(KEY)).toBeNull();
  });

  it('removes the row when the field is simply emptied', () => {
    const { result } = mount();
    act(() => {
      result.current.api.setText('sess-1', 'typed');
      vi.advanceTimersByTime(400);
    });
    act(() => {
      result.current.api.setText('sess-1', '');
      vi.advanceTimersByTime(400);
    });
    expect(window.localStorage.getItem(KEY)).toBeNull();
  });

  it('never writes a row for the conversation-less /chat bucket, but persists it on adoption', () => {
    const { result } = mount();
    act(() => {
      result.current.api.setText('', 'typed before it opened');
      vi.advanceTimersByTime(400);
    });
    expect(window.localStorage.getItem('baybo.draft.')).toBeNull();
    act(() => {
      result.current.api.adopt('', 'sess-1');
    });
    // Adoption is terminal for the source, so it writes through rather than
    // waiting on the debounce.
    expect(stored()?.text).toBe('typed before it opened');
  });

  it('writes the removal through on send, without waiting for the debounce', () => {
    const { result } = mount();
    act(() => {
      result.current.api.setText('sess-1', 'deploy now');
      vi.advanceTimersByTime(400);
    });
    expect(stored()).not.toBeNull();
    act(() => {
      result.current.api.discard('sess-1');
    });
    // No timer advance, no pagehide, no unmount — this is the tab dying
    // ungracefully right after Enter. The row must already be gone, or the
    // reload restores a message that was sent (and, on the park path, is
    // already sitting in `baybo.queue.<id>`) and the user sends it twice.
    expect(window.localStorage.getItem(KEY)).toBeNull();
  });
});

describe('DraftProvider — a pick is addressed by its localId', () => {
  const pick = (localId: string, previewUrl?: string): PendingAttachment => ({
    localId,
    filename: `${localId}.png`,
    mime: 'image/png',
    size: 4,
    status: 'uploading',
    previewUrl,
  });

  it('settles an upload into the conversation its draft was adopted into', () => {
    const { result } = mount();
    act(() => {
      // Staged at `/chat`, then the bootstrap redirect lands on a conversation
      // while the POST /v1/blobs is still in flight.
      result.current.api.stage('', pick('p1'));
      result.current.api.adopt('', 'sess-1');
      result.current.api.settle('', 'p1', { status: 'ready', blobId: 'blob-1' });
    });
    const att = result.current.drafts['sess-1']?.attachments[0];
    // Missing this leaves the pick on `uploading` forever, and both the send
    // gate and the send button read exactly that — a dead composer.
    expect(att?.status).toBe('ready');
    expect(result.current.drafts['']).toBeUndefined();
  });

  it('does not resurrect a pick whose draft was sent while it was uploading', () => {
    const { result } = mount();
    act(() => {
      result.current.api.stage('sess-1', pick('p1'));
      result.current.api.discard('sess-1');
      result.current.api.settle('sess-1', 'p1', { status: 'ready', blobId: 'blob-1' });
    });
    expect(result.current.drafts['sess-1']).toBeUndefined();
  });

  it('drops one pick and revokes only that pick\'s preview', () => {
    const revoke = vi.spyOn(URL, 'revokeObjectURL').mockImplementation(() => {});
    const { result } = mount();
    act(() => {
      result.current.api.stage('sess-1', pick('p1', 'blob:one'));
      result.current.api.stage('sess-1', pick('p2', 'blob:two'));
      result.current.api.drop('sess-1', 'p1');
    });
    expect(result.current.drafts['sess-1']?.attachments.map((a) => a.localId)).toEqual(['p2']);
    expect(revoke.mock.calls).toEqual([['blob:one']]);
  });

  it('revokes every preview a discarded draft was holding', () => {
    const revoke = vi.spyOn(URL, 'revokeObjectURL').mockImplementation(() => {});
    const { result } = mount();
    act(() => {
      result.current.api.stage('sess-1', pick('p1', 'blob:one'));
      result.current.api.stage('sess-1', pick('p2', 'blob:two'));
      result.current.api.discard('sess-1');
    });
    expect(revoke.mock.calls.flat().sort()).toEqual(['blob:one', 'blob:two']);
  });

  it('gives a restored pick its re-fetched thumbnail back', () => {
    const { result } = mount();
    act(() => {
      result.current.api.stage('sess-1', pick('p1'));
      result.current.api.settle('sess-1', 'p1', { status: 'ready', blobId: 'blob-1' });
      result.current.api.restorePreview('sess-1', 'p1', 'blob:refetched');
    });
    expect(result.current.drafts['sess-1']?.attachments[0]?.previewUrl).toBe('blob:refetched');
  });

  it('revokes a thumbnail that lost the race instead of leaking it', () => {
    const revoke = vi.spyOn(URL, 'revokeObjectURL').mockImplementation(() => {});
    const { result } = mount();
    act(() => {
      result.current.api.stage('sess-1', pick('p1', 'blob:already'));
      result.current.api.restorePreview('sess-1', 'p1', 'blob:late');
    });
    expect(result.current.drafts['sess-1']?.attachments[0]?.previewUrl).toBe('blob:already');
    expect(revoke.mock.calls).toEqual([['blob:late']]);
  });

  it('revokes a thumbnail for a pick that is gone entirely', () => {
    const revoke = vi.spyOn(URL, 'revokeObjectURL').mockImplementation(() => {});
    const { result } = mount();
    act(() => {
      result.current.api.restorePreview('sess-1', 'nobody', 'blob:orphan');
    });
    expect(revoke.mock.calls).toEqual([['blob:orphan']]);
  });
});
