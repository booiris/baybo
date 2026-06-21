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
import type { WireAttachment } from '@aura/chat-core';

// Per-session "interjection queue": messages the operator parks while a turn
// is in flight (or while the session is paused after a /stop / error). They
// fire one-per-completion (the auto-fire pipeline in ChatPage) or on demand.
// Persisted to localStorage under `aura.queue.<sessionId>` so a reload keeps
// the queue; attachments persist as blob refs and re-fetch their preview.

export type PauseReason = 'cancelled' | 'error' | null;
export interface QueuedItem {
  id: string;
  text: string;
  attachments: WireAttachment[];
}
export interface SessionQueue {
  /** Parked messages shown in the queue panel above the composer. */
  items: QueuedItem[];
  /** Messages the operator clicked send on while the agent was streaming its
   *  final reply: there is no tool boundary left to interject at, so the send
   *  is deferred and the message renders in the thread (after the agent's
   *  output) until the turn completes and it is dispatched. Persisted so a
   *  reload keeps a still-pending deferred send. */
  deferred: QueuedItem[];
  pauseReason: PauseReason;
}

type QueueMap = Record<string, SessionQueue>;

const QUEUE_KEY_PREFIX = 'aura.queue.';
const EMPTY_QUEUE: SessionQueue = Object.freeze({ items: [], deferred: [], pauseReason: null });
const queueKey = (sessionId: string) => `${QUEUE_KEY_PREFIX}${sessionId}`;
const emptyQueue = (): SessionQueue => ({ items: [], deferred: [], pauseReason: null });

function isQueuedItem(it: unknown): it is QueuedItem {
  return (
    typeof it === 'object' &&
    it !== null &&
    typeof (it as QueuedItem).id === 'string' &&
    typeof (it as QueuedItem).text === 'string' &&
    Array.isArray((it as QueuedItem).attachments)
  );
}

function isValidQueue(v: unknown): v is SessionQueue {
  if (typeof v !== 'object' || v === null) return false;
  const q = v as Record<string, unknown>;
  if (!Array.isArray(q.items)) return false;
  // `deferred` is tolerated as absent so a queue persisted before this field
  // existed still loads (coerced to [] in readQueue).
  if (q.deferred !== undefined && !Array.isArray(q.deferred)) return false;
  if (!(q.pauseReason === null || q.pauseReason === 'cancelled' || q.pauseReason === 'error'))
    return false;
  return q.items.every(isQueuedItem) && (q.deferred ?? []).every(isQueuedItem);
}

function readQueue(sessionId: string): SessionQueue {
  try {
    const raw = window.localStorage.getItem(queueKey(sessionId));
    if (raw === null) return emptyQueue();
    const parsed: unknown = JSON.parse(raw);
    if (!isValidQueue(parsed)) return emptyQueue();
    return { items: parsed.items, deferred: parsed.deferred ?? [], pauseReason: parsed.pauseReason };
  } catch {
    return emptyQueue();
  }
}

function writeQueue(sessionId: string, q: SessionQueue): void {
  try {
    if (q.items.length === 0 && q.deferred.length === 0 && q.pauseReason === null) {
      window.localStorage.removeItem(queueKey(sessionId));
      return;
    }
    window.localStorage.setItem(queueKey(sessionId), JSON.stringify(q));
  } catch {
    /* storage disabled; in-memory tracking still works this session */
  }
}

function loadAllQueues(): QueueMap {
  const out: QueueMap = {};
  try {
    for (let i = 0; i < window.localStorage.length; i++) {
      const key = window.localStorage.key(i);
      if (!key || !key.startsWith(QUEUE_KEY_PREFIX)) continue;
      const sid = key.slice(QUEUE_KEY_PREFIX.length);
      out[sid] = readQueue(sid);
    }
  } catch {
    /* storage disabled */
  }
  return out;
}

// A pauseReason is meaningless once the queue is empty: there is nothing left
// to resume, and a lingering pause would silently force-park the next message
// and gate off auto-fire. Collapse it on every mutation so no drain path can
// leave a session stuck-paused with zero items.
function normalize(q: SessionQueue): SessionQueue {
  return q.items.length === 0 && q.pauseReason !== null ? { ...q, pauseReason: null } : q;
}

// Imperative, session-explicit handle. `queue()` reads the live ref (current
// value, not a render snapshot) for the WS onFrame closure; the mutators are
// stable. This context never re-renders its consumers on queue change.
interface QueueApi {
  queue: (sessionId: string) => SessionQueue;
  enqueue: (sessionId: string, item: QueuedItem) => void;
  removeItem: (sessionId: string, id: string) => void;
  editItem: (sessionId: string, id: string, text: string) => void;
  reorder: (sessionId: string, orderedIds: string[]) => void;
  popTop: (sessionId: string) => QueuedItem | undefined;
  takeItem: (sessionId: string, id: string) => QueuedItem | undefined;
  /** Move a parked item into the deferred ("send after the reply") list. */
  deferItem: (sessionId: string, id: string) => void;
  /** Drop a deferred item once it has been dispatched. */
  removeDeferred: (sessionId: string, id: string) => void;
  /** Move every deferred item back to the front of the parked queue — used on
   *  `/stop`, where the reply they were waiting on was cancelled. */
  restoreDeferred: (sessionId: string) => void;
  setPause: (sessionId: string, reason: Exclude<PauseReason, null>) => void;
  clearPause: (sessionId: string) => void;
}

const QueueApiContext = createContext<QueueApi | null>(null);
// Reactive slice: hooks subscribe to this so { items, pauseReason } and the
// sidebar counts re-render on mutation. Kept separate from QueueApiContext so
// the imperative handle stays referentially stable.
const QueueStateContext = createContext<QueueMap>({});

export function QueueProvider({ children }: { children: ReactNode }) {
  const [queues, setQueues] = useState<QueueMap>(() => loadAllQueues());
  const queuesRef = useRef<QueueMap>(queues);
  queuesRef.current = queues;

  // apply() updates queuesRef.current SYNCHRONOUSLY before setQueues so a
  // second mutation in the SAME tick (resume = clearPause then popTop, or
  // auto-fire peek then popTop) composes off the first instead of clobbering
  // it through the stale render-time ref.
  const apply = useCallback((sessionId: string, fn: (cur: SessionQueue) => SessionQueue): void => {
    const cur = queuesRef.current[sessionId] ?? emptyQueue();
    const next = normalize(fn(cur));
    writeQueue(sessionId, next);
    queuesRef.current = { ...queuesRef.current, [sessionId]: next };
    setQueues(queuesRef.current);
  }, []);

  const queue = useCallback(
    (sessionId: string): SessionQueue => queuesRef.current[sessionId] ?? EMPTY_QUEUE,
    [],
  );
  const enqueue = useCallback(
    (sessionId: string, item: QueuedItem) =>
      apply(sessionId, (cur) => ({ ...cur, items: [...cur.items, item] })),
    [apply],
  );
  const removeItem = useCallback(
    (sessionId: string, id: string) =>
      apply(sessionId, (cur) => ({ ...cur, items: cur.items.filter((i) => i.id !== id) })),
    [apply],
  );
  const editItem = useCallback(
    (sessionId: string, id: string, text: string) =>
      apply(sessionId, (cur) => ({
        ...cur,
        items: cur.items.map((i) => (i.id === id ? { ...i, text } : i)),
      })),
    [apply],
  );
  const reorder = useCallback(
    (sessionId: string, orderedIds: string[]) =>
      apply(sessionId, (cur) => {
        const byId = new Map(cur.items.map((i) => [i.id, i] as const));
        const items = orderedIds
          .map((id) => byId.get(id))
          .filter((i): i is QueuedItem => i !== undefined);
        return { ...cur, items };
      }),
    [apply],
  );
  const popTop = useCallback(
    (sessionId: string): QueuedItem | undefined => {
      const top = queuesRef.current[sessionId]?.items[0];
      if (!top) return undefined;
      apply(sessionId, (cur) => ({ ...cur, items: cur.items.slice(1) }));
      return top;
    },
    [apply],
  );
  const takeItem = useCallback(
    (sessionId: string, id: string): QueuedItem | undefined => {
      const found = queuesRef.current[sessionId]?.items.find((i) => i.id === id);
      if (!found) return undefined;
      apply(sessionId, (cur) => ({ ...cur, items: cur.items.filter((i) => i.id !== id) }));
      return found;
    },
    [apply],
  );
  const deferItem = useCallback(
    (sessionId: string, id: string) =>
      apply(sessionId, (cur) => {
        const found = cur.items.find((i) => i.id === id);
        if (!found) return cur;
        return {
          ...cur,
          items: cur.items.filter((i) => i.id !== id),
          deferred: [...cur.deferred, found],
        };
      }),
    [apply],
  );
  const removeDeferred = useCallback(
    (sessionId: string, id: string) =>
      apply(sessionId, (cur) => ({
        ...cur,
        deferred: cur.deferred.filter((i) => i.id !== id),
      })),
    [apply],
  );
  const restoreDeferred = useCallback(
    (sessionId: string) =>
      apply(sessionId, (cur) =>
        cur.deferred.length === 0
          ? cur
          : { ...cur, items: [...cur.deferred, ...cur.items], deferred: [] },
      ),
    [apply],
  );
  const setPause = useCallback(
    (sessionId: string, reason: Exclude<PauseReason, null>) =>
      apply(sessionId, (cur) => ({ ...cur, pauseReason: reason })),
    [apply],
  );
  const clearPause = useCallback(
    (sessionId: string) => apply(sessionId, (cur) => ({ ...cur, pauseReason: null })),
    [apply],
  );

  // Ingest writes from OTHER tabs (the storage event never fires in the writer
  // tab) so sidebars/panels stay in sync across tabs of the same origin.
  useEffect(() => {
    const onStorage = (e: StorageEvent) => {
      if (!e.key || !e.key.startsWith(QUEUE_KEY_PREFIX)) return;
      const sid = e.key.slice(QUEUE_KEY_PREFIX.length);
      const next = e.newValue === null ? emptyQueue() : readQueue(sid);
      queuesRef.current = { ...queuesRef.current, [sid]: next };
      setQueues(queuesRef.current);
    };
    window.addEventListener('storage', onStorage);
    return () => window.removeEventListener('storage', onStorage);
  }, []);

  const api = useMemo<QueueApi>(
    () => ({
      queue,
      enqueue,
      removeItem,
      editItem,
      reorder,
      popTop,
      takeItem,
      deferItem,
      removeDeferred,
      restoreDeferred,
      setPause,
      clearPause,
    }),
    [
      queue,
      enqueue,
      removeItem,
      editItem,
      reorder,
      popTop,
      takeItem,
      deferItem,
      removeDeferred,
      restoreDeferred,
      setPause,
      clearPause,
    ],
  );

  return (
    <QueueApiContext.Provider value={api}>
      <QueueStateContext.Provider value={queues}>{children}</QueueStateContext.Provider>
    </QueueApiContext.Provider>
  );
}

// Imperative handle for the WS onFrame closure (capture in a ref). Does NOT
// re-render on queue change — read `queue(sid)` for the live value.
export function useQueueStore(): QueueApi {
  const ctx = useContext(QueueApiContext);
  if (!ctx) throw new Error('useQueueStore must be used inside <QueueProvider>');
  return ctx;
}

// Per-session ergonomic hook: REACTIVE { items, pauseReason } from context
// state + session-BOUND mutations (no sessionId arg). `sessionId` undefined →
// empty + no-op mutations.
export function useSessionQueue(sessionId: string | undefined) {
  const api = useQueueStore();
  const queues = useContext(QueueStateContext);
  const q = (sessionId && queues[sessionId]) || EMPTY_QUEUE;
  const enqueue = useCallback(
    (item: QueuedItem) => {
      if (sessionId) api.enqueue(sessionId, item);
    },
    [api, sessionId],
  );
  const removeItem = useCallback(
    (id: string) => {
      if (sessionId) api.removeItem(sessionId, id);
    },
    [api, sessionId],
  );
  const editItem = useCallback(
    (id: string, text: string) => {
      if (sessionId) api.editItem(sessionId, id, text);
    },
    [api, sessionId],
  );
  const reorder = useCallback(
    (ids: string[]) => {
      if (sessionId) api.reorder(sessionId, ids);
    },
    [api, sessionId],
  );
  const popTop = useCallback(
    () => (sessionId ? api.popTop(sessionId) : undefined),
    [api, sessionId],
  );
  const takeItem = useCallback(
    (id: string) => (sessionId ? api.takeItem(sessionId, id) : undefined),
    [api, sessionId],
  );
  const deferItem = useCallback(
    (id: string) => {
      if (sessionId) api.deferItem(sessionId, id);
    },
    [api, sessionId],
  );
  const removeDeferred = useCallback(
    (id: string) => {
      if (sessionId) api.removeDeferred(sessionId, id);
    },
    [api, sessionId],
  );
  const setPause = useCallback(
    (reason: Exclude<PauseReason, null>) => {
      if (sessionId) api.setPause(sessionId, reason);
    },
    [api, sessionId],
  );
  const clearPause = useCallback(() => {
    if (sessionId) api.clearPause(sessionId);
  }, [api, sessionId]);
  return {
    items: q.items,
    deferred: q.deferred,
    pauseReason: q.pauseReason,
    enqueue,
    removeItem,
    editItem,
    reorder,
    popTop,
    takeItem,
    deferItem,
    removeDeferred,
    setPause,
    clearPause,
  };
}

// Reactive per-session queue counts for the sidebar badge — parked items plus
// any deferred ("send after the reply") messages still pending dispatch.
// Independent of WS subscription.
export function useQueueCounts(): Map<string, number> {
  const queues = useContext(QueueStateContext);
  return useMemo(() => {
    const m = new Map<string, number>();
    for (const [id, q] of Object.entries(queues)) {
      const count = q.items.length + q.deferred.length;
      if (count > 0) m.set(id, count);
    }
    return m;
  }, [queues]);
}
