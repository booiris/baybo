import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from 'react';

// Per-conversation **unsent composer state**: what the user typed and what they
// staged, kept while they read another conversation, while the transcript
// bucket behind it is evicted, and while they are off on an admin page.
//
// Exactly two things END a draft: the message is sent, and the conversation is
// hidden. Everything else — switching conversations, an LRU eviction of the
// transcript (`VIEW_CACHE_LIMIT`), a trip to /logs and back, archiving — is a
// checkpoint, and the draft comes back on the next visit. The iOS composer
// states the same rule (`app/ios/docs/attachments.md` § Leaving is not
// discarding) and models the same record; this is its web counterpart.
//
// Mounted ABOVE the router (`main.tsx`, beside `QueueProvider`) rather than
// inside `ChatPage`, because the always-mounted icon rail unmounts `ChatPage`
// on every trip to an admin route — component state would drop every draft in
// every conversation on a glance at the log.
//
// Persisted to `localStorage` under `baybo.draft.<sessionId>`, one row per
// conversation, beside `baybo.queue.<sessionId>` and `baybo.outbox.<sessionId>`
// — so a reload, a crash, or taking the PWA's update offer gives the draft back.
//
// What crosses a reload is text plus the picks that already REACHED THE BLOB
// STORE. A `ready` pick is a `blobId` and the bytes are the gateway's, so the
// chip rebuilds and the send still works; the thumbnail is re-fetched (see
// `restorePreview`) rather than left as a bare filename. A pick still uploading
// or failed is only alive as a local `File` and an object URL, both of which die
// with the document — the browser has no equivalent of the hard-linked spool
// iOS keeps those bytes in (`app/ios/App/Core/DraftStore.swift`), so they are
// dropped on load rather than restored as chips that can never finish.
//
// Writes are DEBOUNCED: this is per-keystroke state, where the queue's
// per-mutation precedent is not, and a pasted wall of text would otherwise be
// re-serialised on every character. Anything the tab can see coming — a hide, a
// pagehide, unmount — flushes instead of waiting.
//
// The `storage` event is deliberately NOT listened to. A queue is shared intent
// and converges across tabs; a draft is what THIS tab's textarea is showing, and
// adopting a sibling tab's keystrokes would rewrite the box under someone's
// hands. Two tabs drafting the same conversation each keep their own; the last
// one to write is what a later reload finds.

/** A file the user picked in the composer. Uploaded to the blob store as soon
 *  as it's selected; a `ready` pick carries the `blobId` that lets it ride this
 *  conversation's next outgoing message. The status/blobId pair is a union so
 *  a `ready` pick without a blob cannot be spelled. */
export type PendingAttachment = {
  localId: string;
  filename: string;
  mime: string;
  size: number;
  /** Local object URL for an instant composer thumbnail (images only). Created
   *  by the composer at staging; revoked here, by whichever of `drop` /
   *  `discard` takes the pick out of its draft. */
  previewUrl?: string;
} & ({ status: 'uploading' | 'error' } | { status: 'ready'; blobId: string });

/** How an upload finished. `settle` takes this rather than a partial patch so
 *  the ready-without-a-blob state has no spelling. */
export type UploadOutcome = { status: 'ready'; blobId: string } | { status: 'error' };

/** What the composer is holding for one conversation and has not sent. Text and
 *  files are ONE record: a submit posts both into the conversation they were
 *  typed in, so a strip that could drift away from its text would post one
 *  conversation's screenshot under another's words. */
export interface SessionDraft {
  text: string;
  attachments: PendingAttachment[];
}

export type DraftMap = Partial<Record<string, SessionDraft>>;

/** One shared empty list, so an untouched draft keeps a stable `attachments`
 *  identity and the composer callbacks that depend on it do not churn. */
const NO_ATTACHMENTS: PendingAttachment[] = [];

export const EMPTY_DRAFT: SessionDraft = Object.freeze({
  text: '',
  attachments: NO_ATTACHMENTS,
});

/** The bucket for the conversation-less `/chat` surface. That route is real
 *  (`App.tsx`) and its composer is fully typeable, and the window is not a
 *  flicker: the rail's own Chat link lands there, and so does hiding the last
 *  conversation. A matched `/chat/:sessionId` segment can never be empty, so
 *  the empty string cannot collide with a session id. */
export const NO_SESSION_DRAFT_KEY = '';

/** The one bucket that does NOT persist. It exists to carry what you typed into
 *  the conversation you are about to land in, which is a hand-off inside one
 *  visit; a stored copy turns it into a surprise — the row outlives the visit,
 *  and the next load's bootstrap redirect hands a sentence you typed days ago to
 *  whichever unrelated conversation happens to resolve first and be empty. Every
 *  draft that belongs to an actual conversation persists. */
const persists = (key: string) => key !== NO_SESSION_DRAFT_KEY;

export const draftKeyFor = (sessionId: string | undefined): string =>
  sessionId ?? NO_SESSION_DRAFT_KEY;

/** Whether a draft still holds anything. Deliberately NOT trimmed: the map is
 *  the textarea's source of truth, so treating whitespace as empty would delete
 *  the bucket mid-keystroke and erase the spaces as the user typed them. The
 *  send gate does the trimming (`hasContent`). */
function isEmptyDraft(draft: SessionDraft): boolean {
  return draft.text.length === 0 && draft.attachments.length === 0;
}

export function draftAt(drafts: DraftMap, key: string): SessionDraft {
  return drafts[key] ?? EMPTY_DRAFT;
}

/** Apply `patch` to one conversation's draft, leaving every other conversation
 *  reference-identical.
 *
 *  Two rules the callers depend on. A draft that comes back empty is DELETED,
 *  so the map holds exactly the conversations with unsent content. And a patch
 *  that lands on an ABSENT key and comes back empty returns the same map, so an
 *  upload settling after its draft was sent or hidden cannot resurrect a bucket
 *  no screen can reach. */
export function patchDraft(
  drafts: DraftMap,
  key: string,
  patch: (draft: SessionDraft) => SessionDraft,
): DraftMap {
  const before = draftAt(drafts, key);
  const after = patch(before);
  if (after === before) return drafts;
  if (isEmptyDraft(after)) return discardDraft(drafts, key);
  return { ...drafts, [key]: after };
}

/** Drop one conversation's draft. Returns the same map when there was none, so
 *  a hide of a conversation nobody typed in re-renders nothing. */
export function discardDraft(drafts: DraftMap, key: string): DraftMap {
  if (!(key in drafts)) return drafts;
  const rest = { ...drafts };
  delete rest[key];
  return rest;
}

/** Hand the no-session draft to the conversation the user lands in — but only
 *  when that conversation has nothing typed of its own. The source is never
 *  destroyed by a refusal: it stays put and is found again at `/chat`. */
export function adoptDraft(drafts: DraftMap, fromKey: string, toKey: string): DraftMap {
  if (fromKey === toKey) return drafts;
  const incoming = drafts[fromKey];
  if (!incoming || toKey in drafts) return drafts;
  const next = { ...drafts, [toKey]: incoming };
  delete next[fromKey];
  return next;
}

/** Every preview object URL a draft holds — what a caller must revoke when the
 *  draft (or one of its picks) stops being reachable. */
function previewUrlsOf(attachments: PendingAttachment[]): string[] {
  return attachments.flatMap((a) => (a.previewUrl === undefined ? [] : [a.previewUrl]));
}

/** Which conversation actually holds a pick. A pick's identity is its `localId`
 *  (a uuid, so the search cannot collide); `key` is only where the caller
 *  staged it, and is right unless the draft has been RE-KEYED since — which
 *  `adopt` does, moving the whole record, uploads included. A settle that
 *  missed would leave that pick on `uploading` forever, and both the send
 *  gate and the send button read exactly that, so the conversation's composer
 *  would be dead with no visible cause. `undefined` means the pick is genuinely
 *  gone (its draft sent or its conversation hidden) and must not come back. */
function holderOf(drafts: DraftMap, key: string, localId: string): string | undefined {
  const holds = (k: string) => drafts[k]?.attachments.some((a) => a.localId === localId) === true;
  if (holds(key)) return key;
  return Object.keys(drafts).find(holds);
}

// ── Persistence ─────────────────────────────────────────────────────────────

const DRAFT_KEY_PREFIX = 'baybo.draft.';
const storageKey = (key: string) => `${DRAFT_KEY_PREFIX}${key}`;

/** How long a draft may sit unwritten. Long enough that a burst of typing costs
 *  one write, short enough that a tab killed from the outside loses at most a
 *  word. Every exit the tab can see coming flushes instead of waiting. */
const DRAFT_PERSIST_DEBOUNCE_MS = 400;

/** A draft as it survives a reload: the text, and only the picks whose bytes are
 *  already the gateway's. `previewUrl` is absent by construction — an object URL
 *  belongs to the document that minted it. */
interface StoredDraft {
  text: string;
  attachments: { localId: string; filename: string; mime: string; size: number; blobId: string }[];
}

function toStored(draft: SessionDraft): StoredDraft {
  return {
    text: draft.text,
    attachments: draft.attachments.flatMap((a) =>
      a.status === 'ready'
        ? [
            {
              localId: a.localId,
              filename: a.filename,
              mime: a.mime,
              size: a.size,
              blobId: a.blobId,
            },
          ]
        : [],
    ),
  };
}

function isStoredDraft(v: unknown): v is StoredDraft {
  if (typeof v !== 'object' || v === null) return false;
  const d = v as Record<string, unknown>;
  if (typeof d.text !== 'string' || !Array.isArray(d.attachments)) return false;
  return d.attachments.every((a: unknown) => {
    if (typeof a !== 'object' || a === null) return false;
    const r = a as Record<string, unknown>;
    return (
      typeof r.localId === 'string' &&
      typeof r.filename === 'string' &&
      typeof r.mime === 'string' &&
      typeof r.size === 'number' &&
      typeof r.blobId === 'string'
    );
  });
}

function fromStored(stored: StoredDraft): SessionDraft {
  return {
    text: stored.text,
    attachments: stored.attachments.map((a) => ({ ...a, status: 'ready' })),
  };
}

/** Every draft this browser has for this origin. Read once, before the first
 *  render, so a restored conversation paints its draft rather than flashing an
 *  empty box and filling it a frame later. */
export function loadDrafts(): DraftMap {
  const out: DraftMap = {};
  try {
    for (let i = 0; i < window.localStorage.length; i++) {
      const raw = window.localStorage.key(i);
      if (raw === null || !raw.startsWith(DRAFT_KEY_PREFIX)) continue;
      const key = raw.slice(DRAFT_KEY_PREFIX.length);
      if (!persists(key)) continue;
      // Per row, not around the sweep: one unparseable row must cost its own
      // conversation's draft and nobody else's.
      let stored: unknown;
      try {
        stored = JSON.parse(window.localStorage.getItem(raw) ?? 'null');
      } catch {
        continue;
      }
      if (!isStoredDraft(stored)) continue;
      const draft = fromStored(stored);
      // A row holding nothing but picks that died with their document is not a
      // draft any more; drop it instead of restoring an empty bucket.
      if (!isEmptyDraft(draft)) out[key] = draft;
    }
  } catch {
    /* storage disabled or unreadable — this session is simply in-memory */
  }
  return out;
}

function persistDraft(key: string, draft: SessionDraft | undefined): void {
  if (!persists(key)) return;
  try {
    const stored = draft === undefined ? undefined : toStored(draft);
    if (stored === undefined || (stored.text.length === 0 && stored.attachments.length === 0)) {
      window.localStorage.removeItem(storageKey(key));
      return;
    }
    window.localStorage.setItem(storageKey(key), JSON.stringify(stored));
  } catch {
    /* full or blocked store — the draft stays live for this session */
  }
}

/** Mutators. Each names the conversation it writes into: the composer's own
 *  writes pass the key they rendered for, and `uploadAttachment` passes the key
 *  it captured when the pick was staged, so an upload that finishes after the
 *  user has moved on still settles the chip it created. */
export interface DraftApi {
  setText(key: string, text: string): void;
  stage(key: string, attachment: PendingAttachment): void;
  settle(key: string, localId: string, outcome: UploadOutcome): void;
  drop(key: string, localId: string): void;
  discard(key: string): void;
  adopt(fromKey: string, toKey: string): void;
  /** Give a restored pick back its thumbnail. The store cannot fetch — the blob
   *  read is bearer-gated — so the composer hands the object URL in, and the
   *  store takes over its lifetime like any other preview. */
  restorePreview(key: string, localId: string, previewUrl: string): void;
}

const DraftStateContext = createContext<DraftMap>({});
const DraftApiContext = createContext<DraftApi | null>(null);

export function DraftProvider({ children }: { children: ReactNode }) {
  // Read before the first render, so a restored draft paints with the composer
  // instead of appearing a frame later.
  const [drafts, setDrafts] = useState<DraftMap>(loadDrafts);
  // Seeded from the state's own object so the two start identical.
  // Updated synchronously by `apply`, ahead of the render it schedules, so two
  // mutations in one tick compose off the first — and so a mutator can see the
  // attachments it is about to drop in order to revoke their object URLs
  // OUTSIDE a state updater (React may run an updater twice and discard one
  // result; revoking in there would kill a live <img> in dev).
  const draftsRef = useRef<DraftMap>(drafts);

  // Conversations whose row is behind the map, and the timer that will catch it
  // up. Keys rather than values: the writer reads whatever the map says when it
  // finally runs, so a burst of edits collapses into one write per conversation.
  const dirtyRef = useRef<Set<string>>(new Set());
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const flush = useCallback(() => {
    if (timerRef.current !== null) {
      clearTimeout(timerRef.current);
      timerRef.current = null;
    }
    for (const key of dirtyRef.current) persistDraft(key, draftsRef.current[key]);
    dirtyRef.current.clear();
  }, []);

  const schedulePersist = useCallback(
    (key: string) => {
      dirtyRef.current.add(key);
      if (timerRef.current !== null) return;
      timerRef.current = setTimeout(flush, DRAFT_PERSIST_DEBOUNCE_MS);
    },
    [flush],
  );

  // Anything the tab can see coming writes now rather than losing the tail of
  // what was typed: a reload, a close, and the backgrounding that on mobile is
  // the last callback before the tab is discarded outright.
  useEffect(() => {
    const onHide = () => {
      if (document.visibilityState === 'hidden') flush();
    };
    window.addEventListener('pagehide', flush);
    document.addEventListener('visibilitychange', onHide);
    return () => {
      window.removeEventListener('pagehide', flush);
      document.removeEventListener('visibilitychange', onHide);
      flush();
    };
  }, [flush]);

  const apply = useCallback(
    (next: DraftMap, touched: string[], immediate = false) => {
      // Ahead of the write-through below: `discardDraft` hands back the same map
      // when the key was absent, and removing a row on that would delete what a
      // sibling tab just wrote for a conversation this one never typed in.
      if (next === draftsRef.current) return;
      draftsRef.current = next;
      setDrafts(next);
      for (const key of touched) {
        if (!immediate) {
          schedulePersist(key);
          continue;
        }
        // Terminal events write through. The debounce exists to amortise
        // per-keystroke serialisation, and sending or hiding is one-shot — but
        // more than that, the queue writes `baybo.queue.<id>` synchronously
        // (`queueStore`), so a parked message is durable the instant Enter
        // returns. Leaving the draft row to a timer opens a window where an
        // ungraceful exit (a renderer OOM, a kill -9) restores a draft the user
        // already sent, next to the queue row holding the same text.
        dirtyRef.current.delete(key);
        persistDraft(key, next[key]);
      }
    },
    [schedulePersist],
  );

  const mutate = useCallback(
    (key: string, patch: (draft: SessionDraft) => SessionDraft) => {
      apply(patchDraft(draftsRef.current, key, patch), [key]);
    },
    [apply],
  );

  const api = useMemo<DraftApi>(
    () => ({
      setText: (key, text) => mutate(key, (d) => (d.text === text ? d : { ...d, text })),

      stage: (key, attachment) =>
        mutate(key, (d) => ({ ...d, attachments: [...d.attachments, attachment] })),

      settle: (key, localId, outcome) => {
        const holder = holderOf(draftsRef.current, key, localId);
        if (holder === undefined) return;
        mutate(holder, (d) => ({
          ...d,
          attachments: d.attachments.map((a) => (a.localId === localId ? { ...a, ...outcome } : a)),
        }));
      },

      drop: (key, localId) => {
        const target = draftAt(draftsRef.current, key).attachments.find(
          (a) => a.localId === localId,
        );
        mutate(key, (d) => {
          const rest = d.attachments.filter((a) => a.localId !== localId);
          return rest.length === d.attachments.length ? d : { ...d, attachments: rest };
        });
        if (target !== undefined) previewUrlsOf([target]).forEach((url) => URL.revokeObjectURL(url));
      },

      discard: (key) => {
        const leaving = draftAt(draftsRef.current, key).attachments;
        apply(discardDraft(draftsRef.current, key), [key], true);
        previewUrlsOf(leaving).forEach((url) => URL.revokeObjectURL(url));
      },

      adopt: (fromKey, toKey) =>
        apply(adoptDraft(draftsRef.current, fromKey, toKey), [fromKey, toKey], true),

      restorePreview: (key, localId, previewUrl) => {
        const holder = holderOf(draftsRef.current, key, localId);
        const target =
          holder === undefined
            ? undefined
            : draftAt(draftsRef.current, holder).attachments.find((a) => a.localId === localId);
        // Lost the race with a remove, a send, or a preview that already landed
        // — the caller handed its URL over, so revoke it rather than leak it.
        if (holder === undefined || target === undefined || target.previewUrl !== undefined) {
          URL.revokeObjectURL(previewUrl);
          return;
        }
        mutate(holder, (d) => ({
          ...d,
          attachments: d.attachments.map((a) => (a.localId === localId ? { ...a, previewUrl } : a)),
        }));
      },
    }),
    [apply, mutate],
  );

  return (
    <DraftApiContext.Provider value={api}>
      <DraftStateContext.Provider value={drafts}>{children}</DraftStateContext.Provider>
    </DraftApiContext.Provider>
  );
}

/** The reactive map — re-renders on every keystroke, so only the composer's own
 *  page should read it. */
export function useDrafts(): DraftMap {
  return useContext(DraftStateContext);
}

/** The referentially-stable mutator handle. Safe to capture in a long-lived
 *  closure (the chat socket's `onFrame` hides a conversation from another tab)
 *  and to list in an effect's dependencies without churning it. */
export function useDraftApi(): DraftApi {
  const api = useContext(DraftApiContext);
  if (api === null) throw new Error('useDraftApi must be used within a DraftProvider');
  return api;
}
