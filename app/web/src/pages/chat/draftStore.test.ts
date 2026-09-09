import { beforeEach, describe, expect, it } from 'vitest';

import {
  adoptDraft,
  discardDraft,
  draftAt,
  draftKeyFor,
  loadDrafts,
  patchDraft,
  EMPTY_DRAFT,
  NO_SESSION_DRAFT_KEY,
  type DraftMap,
  type PendingAttachment,
  type SessionDraft,
} from './draftStore';
import { installMemoryLocalStorage } from '../../test/memoryStorage';

// The map algebra behind the per-conversation composer. What it defends is the
// reported bug expressed as data — a draft typed in one conversation cannot
// appear in, or disturb, another — plus the three rules the composer's callers
// lean on: a draft that empties out is deleted, an upload settling after its
// draft is gone cannot resurrect a bucket no screen can reach, and a text edit
// leaves the staged strip reference-identical so the composer's callbacks do
// not churn on every keystroke.
//
// Source of truth for the lifecycle: the iOS composer, which keys the same
// record by session id (`app/ios/App/Core/DraftStore.swift`) — exactly two
// things end a draft, sending it and deleting the conversation.

const withText = (text: string) => (d: SessionDraft): SessionDraft => ({ ...d, text });

const pick = (localId: string): PendingAttachment => ({
  localId,
  filename: `${localId}.png`,
  mime: 'image/png',
  size: 4,
  status: 'uploading',
});

const stage = (att: PendingAttachment) => (d: SessionDraft): SessionDraft => ({
  ...d,
  attachments: [...d.attachments, att],
});

const markReady = (localId: string) => (d: SessionDraft): SessionDraft => ({
  ...d,
  attachments: d.attachments.map((a) =>
    a.localId === localId ? { ...a, status: 'ready', blobId: 'blob-1' } : a,
  ),
});

describe('draftKeyFor', () => {
  it('sends the conversation-less /chat surface to its own bucket', () => {
    expect(draftKeyFor(undefined)).toBe(NO_SESSION_DRAFT_KEY);
    expect(draftKeyFor('sess-1')).toBe('sess-1');
  });
});

describe('draftAt', () => {
  it('answers with the shared empty draft for a conversation nobody typed in', () => {
    const drafts: DraftMap = {};
    expect(draftAt(drafts, 'sess-1')).toBe(EMPTY_DRAFT);
    // Same identity every time, so an untouched composer's `attachments` array
    // never changes under a dependency array.
    expect(draftAt(drafts, 'sess-2').attachments).toBe(draftAt(drafts, 'sess-1').attachments);
  });
});

describe('patchDraft — isolation', () => {
  it('leaves every other conversation reference-identical', () => {
    const before: DraftMap = { A: { text: 'for A', attachments: [] } };
    const after = patchDraft(before, 'B', withText('for B'));
    expect(after.A).toBe(before.A);
    expect(after.B?.text).toBe('for B');
  });

  it('starts an absent conversation from empty without mutating the shared blank', () => {
    const after = patchDraft({}, 'A', withText('hi'));
    expect(after).toEqual({ A: { text: 'hi', attachments: [] } });
    expect(EMPTY_DRAFT).toEqual({ text: '', attachments: [] });
  });

  it('does not mutate the map it was given', () => {
    const before: DraftMap = { A: { text: 'keep', attachments: [] } };
    const snapshot = structuredClone(before);
    patchDraft(before, 'A', withText('changed'));
    expect(before).toEqual(snapshot);
  });

  it('keeps the staged strip identical across a text-only edit', () => {
    const staged = patchDraft({}, 'A', stage(pick('p1')));
    const typed = patchDraft(staged, 'A', withText('look at this'));
    expect(typed.A?.attachments).toBe(staged.A?.attachments);
  });
});

describe('patchDraft — delete on empty', () => {
  it('removes the conversation once nothing is left in it', () => {
    const before = patchDraft({}, 'A', withText('hi'));
    const after = patchDraft(before, 'A', withText(''));
    expect('A' in after).toBe(false);
  });

  it('keeps a whitespace-only draft, because the map is what the textarea shows', () => {
    // Trimming here would delete the bucket mid-keystroke and swallow the
    // spaces as the user typed them. `hasContent` does the trimming instead.
    const after = patchDraft({}, 'A', withText('   '));
    expect(after.A?.text).toBe('   ');
  });

  it('keeps a draft that still holds a file after its text is cleared', () => {
    const staged = patchDraft({}, 'A', stage(pick('p1')));
    const cleared = patchDraft(staged, 'A', withText(''));
    expect(cleared.A?.attachments).toHaveLength(1);
  });

  it('returns the same map when a no-op patch lands on an absent conversation', () => {
    const before: DraftMap = { A: { text: 'hi', attachments: [] } };
    expect(patchDraft(before, 'B', withText(''))).toBe(before);
  });
});

describe('patchDraft — a late upload cannot resurrect a draft', () => {
  it('ignores a settle for a conversation whose draft was already sent or hidden', () => {
    const before: DraftMap = { B: { text: 'untouched', attachments: [] } };
    // `uploadAttachment` resolving after `handleSend` discarded A's draft.
    const after = patchDraft(before, 'A', markReady('p1'));
    expect(after).toBe(before);
    expect('A' in after).toBe(false);
  });

  it('still settles a pick whose draft is alive but no longer on screen', () => {
    const staged = patchDraft({}, 'A', stage(pick('p1')));
    const settled = patchDraft(staged, 'A', markReady('p1'));
    const att = settled.A?.attachments[0];
    expect(att?.status).toBe('ready');
    expect(att?.status === 'ready' ? att.blobId : undefined).toBe('blob-1');
  });
});

describe('discardDraft', () => {
  it('drops one conversation and leaves the rest alone', () => {
    const before: DraftMap = {
      A: { text: 'gone', attachments: [] },
      B: { text: 'stays', attachments: [] },
    };
    const after = discardDraft(before, 'A');
    expect('A' in after).toBe(false);
    expect(after.B).toBe(before.B);
  });

  it('returns the same map when the conversation held nothing', () => {
    const before: DraftMap = { A: { text: 'hi', attachments: [] } };
    expect(discardDraft(before, 'B')).toBe(before);
  });
});

describe('adoptDraft — the no-session draft follows the user in', () => {
  it('hands the /chat draft to the conversation that resolves', () => {
    const before = patchDraft({}, NO_SESSION_DRAFT_KEY, withText('typed before it opened'));
    const after = adoptDraft(before, NO_SESSION_DRAFT_KEY, 'sess-1');
    expect(after['sess-1']?.text).toBe('typed before it opened');
    expect(NO_SESSION_DRAFT_KEY in after).toBe(false);
  });

  it('refuses when the arriving conversation has a draft of its own, and destroys neither', () => {
    let before = patchDraft({}, NO_SESSION_DRAFT_KEY, withText('pending'));
    before = patchDraft(before, 'sess-1', withText('already here'));
    const after = adoptDraft(before, NO_SESSION_DRAFT_KEY, 'sess-1');
    expect(after).toBe(before);
    expect(after['sess-1']?.text).toBe('already here');
    expect(after[NO_SESSION_DRAFT_KEY]?.text).toBe('pending');
  });

  it('is a no-op when there is nothing waiting at /chat', () => {
    const before: DraftMap = { 'sess-1': { text: 'hi', attachments: [] } };
    expect(adoptDraft(before, NO_SESSION_DRAFT_KEY, 'sess-1')).toBe(before);
  });
});

describe('loadDrafts — what survives a reload', () => {
  beforeEach(() => {
    installMemoryLocalStorage();
  });

  const store = (key: string, value: unknown) =>
    window.localStorage.setItem(`baybo.draft.${key}`, JSON.stringify(value));

  it('reads every conversation back, keyed by session id', () => {
    store('sess-1', { text: 'half written', attachments: [] });
    store('sess-2', { text: 'and another', attachments: [] });
    const loaded = loadDrafts();
    expect(loaded['sess-1']?.text).toBe('half written');
    expect(loaded['sess-2']?.text).toBe('and another');
  });

  it('restores a pick that reached the blob store, as ready and thumbnail-less', () => {
    store('sess-1', {
      text: 'look',
      attachments: [
        { localId: 'p1', filename: 'shot.png', mime: 'image/png', size: 12, blobId: 'blob-1' },
      ],
    });
    const att = loadDrafts()['sess-1']?.attachments[0];
    expect(att?.status).toBe('ready');
    expect(att?.status === 'ready' ? att.blobId : undefined).toBe('blob-1');
    // The object URL died with the document that minted it; the composer
    // re-fetches the blob and hands a fresh one back through `restorePreview`.
    expect(att?.previewUrl).toBeUndefined();
  });

  it('ignores a row whose shape does not match, rather than crashing the tab', () => {
    store('sess-1', { text: 42 });
    store('sess-2', { text: 'fine', attachments: [{ localId: 'p1' }] });
    window.localStorage.setItem('baybo.draft.sess-3', 'not json at all');
    store('sess-4', { text: 'kept', attachments: [] });
    expect(Object.keys(loadDrafts())).toEqual(['sess-4']);
  });

  it('drops a row that is left with nothing once its in-flight picks are gone', () => {
    // Written while a pick was uploading: `toStored` keeps no non-ready pick, so
    // what landed was an empty record. Restoring it would mint a bucket with
    // nothing in it.
    store('sess-1', { text: '', attachments: [] });
    expect(loadDrafts()).toEqual({});
  });

  it('never restores the conversation-less /chat bucket', () => {
    // It is a hand-off inside one visit. Restoring it would hand a sentence
    // typed days ago to whichever conversation the next bootstrap resolves to.
    store(NO_SESSION_DRAFT_KEY, { text: 'typed before it opened', attachments: [] });
    expect(loadDrafts()).toEqual({});
  });

  it('leaves keys belonging to the queue and the outbox alone', () => {
    window.localStorage.setItem('baybo.queue.sess-1', JSON.stringify({ items: [] }));
    window.localStorage.setItem('baybo.outbox.sess-1', JSON.stringify({}));
    window.localStorage.setItem('baybo.inputHistory', JSON.stringify(['hi']));
    expect(loadDrafts()).toEqual({});
  });

  it('is empty, not thrown, when storage is unavailable', () => {
    Object.defineProperty(window, 'localStorage', {
      configurable: true,
      get() {
        throw new Error('blocked');
      },
    });
    expect(loadDrafts()).toEqual({});
  });
});
