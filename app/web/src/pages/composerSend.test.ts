import { describe, it, expect } from 'vitest';

import { decideComposerAction } from './ChatPage';

const base = {
  hasContent: true,
  isStop: false,
  hasPendingQuestion: false,
  busy: false,
  paused: false,
};

describe('decideComposerAction', () => {
  it('does nothing without content, whatever the state', () => {
    expect(decideComposerAction({ ...base, hasContent: false })).toBe('noop');
    expect(
      decideComposerAction({
        hasContent: false,
        isStop: true,
        hasPendingQuestion: true,
        busy: true,
        paused: true,
      }),
    ).toBe('noop');
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

  it('routes /stop to stop, bypassing busy, paused, and pending questions', () => {
    expect(decideComposerAction({ ...base, isStop: true })).toBe('stop');
    expect(
      decideComposerAction({
        hasContent: true,
        isStop: true,
        hasPendingQuestion: true,
        busy: true,
        paused: true,
      }),
    ).toBe('stop');
  });

  it.each([true, false])('queues a message while a question is pending (busy=%s)', (busy) => {
    expect(decideComposerAction({ ...base, hasPendingQuestion: true, busy })).toBe('park');
  });
});
