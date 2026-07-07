// Persisted send outbox (protocol v2). One entry per in-flight send, keyed
// by `platform_msg_id`, persisted to localStorage under
// `baybo.outbox.<sessionId>` (same pattern as `baybo.queue.<sessionId>`) so
// a tab close / reload keeps un-durable sends alive.
//
// Confirmation is two-stage (docs/sync-protocol.md "Send path"):
//   * the server's Echo (ordinal-less user Message with the same
//     platform_msg_id) proves TRANSPORT — `sending` → `sent`, in-connection
//     retries stop;
//   * an ordinal-stamped row with the same platform_msg_id (sync / backfill)
//     proves DURABILITY and releases the entry.
// The retry machine: no echo within ECHO_TIMEOUT_MS → one blind resend
// (same key; safe inside the gateway's live dedup window), hard-capped at
// MAX_AUTO_TRANSMISSIONS total, then `failed` + manual retry. A rebased sync
// hides the floor, so unconfirmed entries go `unknown` and resolve via the
// per-key point lookup instead of blind-resending.

import type { WireAttachment } from '../../api/chatWs';

export type OutboxEntryState = 'sending' | 'sent' | 'failed' | 'unknown';

export interface OutboxEntry {
  platformMsgId: string;
  text: string;
  attachments?: WireAttachment[];
  state: OutboxEntryState;
  /** Automatic transmissions so far (initial send counts as 1). */
  transmissions: number;
  createdAt: number;
  lastSentAt: number;
}

export const OUTBOX_KEY_PREFIX = 'baybo.outbox.';
/** No Echo within this window (state still `sending`, connection alive)
 *  triggers the one blind in-connection resend. Durability lag alone never
 *  does — a mid-turn persist legitimately takes minutes. */
export const ECHO_TIMEOUT_MS = 10_000;
/** Hard cap on automatic transmissions per message (initial + retries);
 *  past it the entry goes `failed` and only a manual retry resets it. */
export const MAX_AUTO_TRANSMISSIONS = 3;

const outboxKey = (sessionId: string) => `${OUTBOX_KEY_PREFIX}${sessionId}`;

/** Whether a `sending` entry's echo window has lapsed and it is still
 *  under the automatic-transmission cap — i.e. a blind resend is due. */
export function dueForBlindResend(entry: OutboxEntry, now: number): boolean {
  return (
    entry.state === 'sending' &&
    entry.transmissions < MAX_AUTO_TRANSMISSIONS &&
    now - entry.lastSentAt >= ECHO_TIMEOUT_MS
  );
}

/** Whether a `sending` entry has exhausted its cap without an echo —
 *  i.e. it must flip to `failed`. */
export function resendExhausted(entry: OutboxEntry, now: number): boolean {
  return (
    entry.state === 'sending' &&
    entry.transmissions >= MAX_AUTO_TRANSMISSIONS &&
    now - entry.lastSentAt >= ECHO_TIMEOUT_MS
  );
}

const OUTBOX_STATES: readonly OutboxEntryState[] = ['sending', 'sent', 'failed', 'unknown'];

function isOutboxEntry(v: unknown): v is OutboxEntry {
  if (typeof v !== 'object' || v === null) return false;
  const e = v as Record<string, unknown>;
  return (
    typeof e.platformMsgId === 'string' &&
    e.platformMsgId.length > 0 &&
    typeof e.text === 'string' &&
    (e.attachments === undefined || Array.isArray(e.attachments)) &&
    OUTBOX_STATES.includes(e.state as OutboxEntryState) &&
    typeof e.transmissions === 'number' &&
    typeof e.createdAt === 'number' &&
    typeof e.lastSentAt === 'number'
  );
}

function readEntries(sessionId: string): Map<string, OutboxEntry> {
  const out = new Map<string, OutboxEntry>();
  try {
    const raw = window.localStorage.getItem(outboxKey(sessionId));
    if (raw === null) return out;
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return out;
    for (const item of parsed) {
      if (isOutboxEntry(item)) out.set(item.platformMsgId, item);
    }
  } catch {
    /* storage disabled / corrupt — in-memory tracking still works */
  }
  return out;
}

/** Imperative per-tab outbox handle. Not reactive — the transcript row's
 *  pending/failed flags are driven by the caller alongside each mutation. */
export class OutboxStore {
  private cache = new Map<string, Map<string, OutboxEntry>>();

  private session(sessionId: string): Map<string, OutboxEntry> {
    let entries = this.cache.get(sessionId);
    if (!entries) {
      entries = readEntries(sessionId);
      this.cache.set(sessionId, entries);
    }
    return entries;
  }

  private persist(sessionId: string): void {
    const entries = this.session(sessionId);
    try {
      if (entries.size === 0) {
        window.localStorage.removeItem(outboxKey(sessionId));
      } else {
        window.localStorage.setItem(outboxKey(sessionId), JSON.stringify([...entries.values()]));
      }
    } catch {
      /* storage disabled */
    }
  }

  private update(
    sessionId: string,
    platformMsgId: string,
    fn: (entry: OutboxEntry) => OutboxEntry,
  ): OutboxEntry | undefined {
    const entries = this.session(sessionId);
    const cur = entries.get(platformMsgId);
    if (!cur) return undefined;
    const next = fn(cur);
    entries.set(platformMsgId, next);
    this.persist(sessionId);
    return next;
  }

  entries(sessionId: string): OutboxEntry[] {
    return [...this.session(sessionId).values()];
  }

  get(sessionId: string, platformMsgId: string): OutboxEntry | undefined {
    return this.session(sessionId).get(platformMsgId);
  }

  /** Every session that has at least one live entry — the union of ids
   *  already cached this page-load and ids persisted by a prior one. */
  sessionIds(): string[] {
    const ids = new Set<string>();
    for (const [sid, entries] of this.cache) {
      if (entries.size > 0) ids.add(sid);
    }
    try {
      for (let i = 0; i < window.localStorage.length; i++) {
        const key = window.localStorage.key(i);
        if (key?.startsWith(OUTBOX_KEY_PREFIX)) ids.add(key.slice(OUTBOX_KEY_PREFIX.length));
      }
    } catch {
      /* storage disabled */
    }
    return [...ids];
  }

  /** Every platform_msg_id not yet durability-confirmed (any state — a
   *  `failed` row must stay on screen with its retry affordance). Drives
   *  the REPLACE-overlay rule. */
  unconfirmedIds(sessionId: string): Set<string> {
    return new Set(this.session(sessionId).keys());
  }

  /** Record a fresh send: state `sending`, the transmission already counted
   *  (callers only enroll sends that actually hit the wire). */
  beginSend(
    sessionId: string,
    input: { platformMsgId: string; text: string; attachments?: WireAttachment[] },
    now = Date.now(),
  ): OutboxEntry {
    const entry: OutboxEntry = {
      platformMsgId: input.platformMsgId,
      text: input.text,
      attachments: input.attachments && input.attachments.length > 0 ? input.attachments : undefined,
      state: 'sending',
      transmissions: 1,
      createdAt: now,
      lastSentAt: now,
    };
    this.session(sessionId).set(entry.platformMsgId, entry);
    this.persist(sessionId);
    return entry;
  }

  /** Echo observed: transport confirmed, in-connection retries stop. The
   *  entry is retained until durability confirms. */
  markEchoed(sessionId: string, platformMsgId: string): OutboxEntry | undefined {
    return this.update(sessionId, platformMsgId, (e) =>
      e.state === 'sending' || e.state === 'unknown' ? { ...e, state: 'sent' } : e,
    );
  }

  /** An ordinal-stamped row with this key was observed (sync / backfill /
   *  point lookup): durability confirmed, release the entry. Returns whether
   *  an entry existed (so callers can clear a row's pending/failed flags). */
  confirmDurable(sessionId: string, platformMsgId: string): boolean {
    const entries = this.session(sessionId);
    if (!entries.delete(platformMsgId)) return false;
    this.persist(sessionId);
    return true;
  }

  /** Count one (re)transmission: state back to `sending`, echo timer re-armed. */
  recordTransmission(sessionId: string, platformMsgId: string, now = Date.now()): OutboxEntry | undefined {
    return this.update(sessionId, platformMsgId, (e) => ({
      ...e,
      state: 'sending',
      transmissions: e.transmissions + 1,
      lastSentAt: now,
    }));
  }

  markFailed(sessionId: string, platformMsgId: string): OutboxEntry | undefined {
    return this.update(sessionId, platformMsgId, (e) => ({ ...e, state: 'failed' }));
  }

  /** A rebased page hid the floor — this entry's absence is unknowable from
   *  the page alone. Park it `unknown` pending the point lookup. */
  markUnknown(sessionId: string, platformMsgId: string): OutboxEntry | undefined {
    return this.update(sessionId, platformMsgId, (e) =>
      e.state === 'failed' ? e : { ...e, state: 'unknown' },
    );
  }

  /** Point lookup proved the key absent: the normal retry machine resumes.
   *  `lastSentAt` is zeroed so the next retry tick treats the entry as due
   *  immediately; no transmission is consumed by the lookup itself. */
  resumeSending(sessionId: string, platformMsgId: string): OutboxEntry | undefined {
    return this.update(sessionId, platformMsgId, (e) => ({
      ...e,
      state: 'sending',
      lastSentAt: 0,
    }));
  }

  /** Manual retry from the failed affordance: same platform_msg_id, the
   *  automatic-transmission budget resets. */
  resetForManualRetry(sessionId: string, platformMsgId: string): OutboxEntry | undefined {
    return this.update(sessionId, platformMsgId, (e) => ({
      ...e,
      state: 'sending',
      transmissions: 0,
      lastSentAt: 0,
    }));
  }
}
