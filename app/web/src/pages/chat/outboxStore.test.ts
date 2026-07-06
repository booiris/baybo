import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  ECHO_TIMEOUT_MS,
  MAX_AUTO_TRANSMISSIONS,
  OutboxStore,
  dueForBlindResend,
  resendExhausted,
} from './outboxStore';

const SID = 'sess-1';

// A minimal in-memory localStorage so the store's persistence round-trips
// without a DOM. Only the four methods the store touches are modelled.
function installMemoryStorage(): void {
  const map = new Map<string, string>();
  const mock: Pick<Storage, 'getItem' | 'setItem' | 'removeItem'> & { length: number; key(i: number): string | null } = {
    getItem: (k) => map.get(k) ?? null,
    setItem: (k, v) => void map.set(k, v),
    removeItem: (k) => void map.delete(k),
    get length() {
      return map.size;
    },
    key: (i) => [...map.keys()][i] ?? null,
  };
  vi.stubGlobal('window', { localStorage: mock });
}

describe('outbox store (two-stage confirm + retry machine)', () => {
  beforeEach(() => installMemoryStorage());
  afterEach(() => vi.unstubAllGlobals());

  it('two-stage confirmation: echo → sent, ordinal-stamped row → released', () => {
    const box = new OutboxStore();
    box.beginSend(SID, { platformMsgId: 'k1', text: 'hi' });
    expect(box.get(SID, 'k1')?.state).toBe('sending');
    box.markEchoed(SID, 'k1');
    expect(box.get(SID, 'k1')?.state).toBe('sent'); // transport confirmed, retained
    // Durability confirmation (a sync/backfill row with the same key) releases.
    expect(box.confirmDurable(SID, 'k1')).toBe(true);
    expect(box.get(SID, 'k1')).toBeUndefined();
  });

  it('an echo alone never releases the entry', () => {
    const box = new OutboxStore();
    box.beginSend(SID, { platformMsgId: 'k1', text: 'hi' });
    box.markEchoed(SID, 'k1');
    expect(box.unconfirmedIds(SID).has('k1')).toBe(true);
  });

  it('blind resend is due after the echo window, then the entry fails at the cap', () => {
    const box = new OutboxStore();
    const t0 = 1_000_000;
    box.beginSend(SID, { platformMsgId: 'k1', text: 'hi' }, t0);
    const e0 = box.get(SID, 'k1')!;
    expect(dueForBlindResend(e0, t0 + 1)).toBe(false); // inside the window
    expect(dueForBlindResend(e0, t0 + ECHO_TIMEOUT_MS)).toBe(true);
    // Transmit up to the cap (initial = 1).
    box.recordTransmission(SID, 'k1', t0 + ECHO_TIMEOUT_MS);
    box.recordTransmission(SID, 'k1', t0 + 2 * ECHO_TIMEOUT_MS);
    const capped = box.get(SID, 'k1')!;
    expect(capped.transmissions).toBe(MAX_AUTO_TRANSMISSIONS);
    expect(dueForBlindResend(capped, t0 + 3 * ECHO_TIMEOUT_MS)).toBe(false);
    expect(resendExhausted(capped, t0 + 3 * ECHO_TIMEOUT_MS)).toBe(true);
  });

  it('a rebase floor parks the entry unknown; the point lookup releases or resumes it', () => {
    const box = new OutboxStore();
    box.beginSend(SID, { platformMsgId: 'k1', text: 'hi' });
    box.markUnknown(SID, 'k1');
    expect(box.get(SID, 'k1')?.state).toBe('unknown');
    // provably absent → the retry machine resumes (no transmission consumed)
    box.resumeSending(SID, 'k1');
    const resumed = box.get(SID, 'k1')!;
    expect(resumed.state).toBe('sending');
    expect(resumed.lastSentAt).toBe(0); // due immediately
  });

  it('a failed entry survives markUnknown and resets on manual retry', () => {
    const box = new OutboxStore();
    box.beginSend(SID, { platformMsgId: 'k1', text: 'hi' });
    box.markFailed(SID, 'k1');
    box.markUnknown(SID, 'k1');
    expect(box.get(SID, 'k1')?.state).toBe('failed'); // failed is sticky
    box.resetForManualRetry(SID, 'k1');
    const retried = box.get(SID, 'k1')!;
    expect(retried.state).toBe('sending');
    expect(retried.transmissions).toBe(0); // budget reset
  });

  it('persists across a fresh store instance (survives reload)', () => {
    const a = new OutboxStore();
    a.beginSend(SID, { platformMsgId: 'k1', text: 'hi' });
    const b = new OutboxStore();
    expect(b.get(SID, 'k1')?.text).toBe('hi');
    expect(b.sessionIds()).toContain(SID);
  });
});
