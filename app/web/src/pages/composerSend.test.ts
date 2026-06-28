import { describe, it, expect } from 'vitest';

import { decideComposerAction } from './ChatPage';

// Pins the composer send-vs-park rule for the interjection queue. The rule is
// deliberately INDEPENDENT of how many items are already queued: an idle,
// unpaused submit always sends directly, so a non-empty queue can never stall
// the composer (the regression this guards). Parking happens only while a turn
// is in flight or the pipeline is paused after a /stop/error; `/stop` always
// bypasses.

const base = { hasContent: true, isStop: false, busy: false, paused: false };

describe('decideComposerAction', () => {
  it('does nothing without content, whatever the state', () => {
    expect(decideComposerAction({ ...base, hasContent: false })).toBe('noop');
    expect(decideComposerAction({ hasContent: false, isStop: true, busy: true, paused: true })).toBe(
      'noop',
    );
  });

  it('sends directly when idle and not paused', () => {
    expect(decideComposerAction(base)).toBe('direct');
  });

  it('REGRESSION: idle + unpaused still sends directly regardless of a non-empty queue', () => {
    // The decision takes no queue-size input on purpose — a non-empty queue
    // must NOT flip this to "park" (that was the stall: leftover items made
    // every send park, so nothing ever reached the agent).
    expect(decideComposerAction(base)).toBe('direct');
  });

  it('parks while a turn is in flight (preserve order)', () => {
    expect(decideComposerAction({ ...base, busy: true })).toBe('park');
  });

  it('parks while paused, even when idle', () => {
    expect(decideComposerAction({ ...base, paused: true })).toBe('park');
    expect(decideComposerAction({ ...base, busy: true, paused: true })).toBe('park');
  });

  it('routes /stop to stop, bypassing busy and paused', () => {
    expect(decideComposerAction({ ...base, isStop: true })).toBe('stop');
    expect(decideComposerAction({ hasContent: true, isStop: true, busy: true, paused: true })).toBe(
      'stop',
    );
  });
});
