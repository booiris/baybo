import { describe, expect, it } from 'vitest';

import {
  canBatchDeferred,
  classifyQueueFrame,
  hasSendableContent,
  type QueueFrameCtx,
} from './ChatPage';
import type { QueuedItem } from './chat/queueStore';
import type { Frame } from '../api/chatWs';

// Pins the queue-drain decision (`classifyQueueFrame`) and the deferred
// batch-vs-individual choice (`canBatchDeferred`) extracted from
// `drainQueueOnFrame`. The reducer carries all the rules; the callback in
// ChatPage only performs the side effects the action names. See
// docs/web-unit-tests.md.

const SID = 'sess-1';

function messageFrame(role: 'user' | 'assistant' = 'assistant'): Frame {
  return { kind: 'message', content: '', session_id: SID, user_id: 'u', channel_type: 'web', role };
}
function turnStateFrame(active: boolean): Frame {
  return { kind: 'turn_state', session_id: SID, active };
}
function noticeFrame(text: string, level = 'info', transient?: boolean): Frame {
  return transient === undefined
    ? { kind: 'notice', session_id: SID, level, text }
    : { kind: 'notice', session_id: SID, level, text, transient };
}

// A live, armed session that hasn't fired this turn and holds nothing queued.
const armedIdle: QueueFrameCtx = {
  stopped: false,
  armed: true,
  alreadyFired: false,
  paused: false,
  hasItems: false,
  hasDeferred: false,
};
const ctx = (over: Partial<QueueFrameCtx>): QueueFrameCtx => ({ ...armedIdle, ...over });

// The stable substring of the /stop-cancelled notice (see ChatPage's
// STOP_CANCELLED_NOTICE_MARKER). Any other text is a plain notice.
const CANCELLED = 'Cancelled the in-progress reply';

describe('classifyQueueFrame — completion (assistant message)', () => {
  it('fires the top parked item when armed, unfired, unpaused', () => {
    expect(classifyQueueFrame(messageFrame(), ctx({ hasItems: true }))).toBe('fire');
  });

  it('flushes deferred sends, which take precedence over parked items', () => {
    expect(classifyQueueFrame(messageFrame(), ctx({ hasDeferred: true }))).toBe('fire-deferred');
    expect(
      classifyQueueFrame(messageFrame(), ctx({ hasItems: true, hasDeferred: true })),
    ).toBe('fire-deferred');
  });

  it('does nothing with an empty queue', () => {
    expect(classifyQueueFrame(messageFrame(), armedIdle)).toBe('none');
  });

  it('never fires for a reload redelivery (unarmed) or a re-fire (already fired)', () => {
    expect(classifyQueueFrame(messageFrame(), ctx({ hasItems: true, armed: false }))).toBe('none');
    expect(classifyQueueFrame(messageFrame(), ctx({ hasItems: true, alreadyFired: true }))).toBe(
      'none',
    );
  });

  it('never fires while paused, or on a /stop-salvaged reply', () => {
    expect(classifyQueueFrame(messageFrame(), ctx({ hasItems: true, paused: true }))).toBe('none');
    expect(classifyQueueFrame(messageFrame(), ctx({ hasDeferred: true, stopped: true }))).toBe(
      'none',
    );
  });

  it('ignores the user echo of an inbound message', () => {
    expect(classifyQueueFrame(messageFrame('user'), ctx({ hasItems: true }))).toBe('none');
  });
});

describe('classifyQueueFrame — turn end (turn_state)', () => {
  it('restores still-pending deferred items the completion could not carry', () => {
    expect(classifyQueueFrame(turnStateFrame(false), ctx({ hasDeferred: true }))).toBe(
      'restore-deferred',
    );
  });

  it('leaves them in the thread if this turn already dispatched them', () => {
    expect(
      classifyQueueFrame(turnStateFrame(false), ctx({ hasDeferred: true, alreadyFired: true })),
    ).toBe('none');
  });

  it('does not restore while paused, or with nothing deferred', () => {
    expect(
      classifyQueueFrame(turnStateFrame(false), ctx({ hasDeferred: true, paused: true })),
    ).toBe('none');
    expect(classifyQueueFrame(turnStateFrame(false), armedIdle)).toBe('none');
  });

  it('an active turn_state is not a completion', () => {
    expect(classifyQueueFrame(turnStateFrame(true), ctx({ hasDeferred: true }))).toBe('none');
  });
});

describe('classifyQueueFrame — terminal notice', () => {
  it('pauses (cancelled) on a /stop that cancelled a reply', () => {
    expect(classifyQueueFrame(noticeFrame(CANCELLED), ctx({ hasDeferred: true }))).toBe(
      'pause-cancelled',
    );
  });

  it('pauses (error) on an error-level notice', () => {
    expect(classifyQueueFrame(noticeFrame('boom', 'error'), ctx({ hasItems: true }))).toBe(
      'pause-error',
    );
  });

  it('is inert when the queue is empty', () => {
    expect(classifyQueueFrame(noticeFrame(CANCELLED), armedIdle)).toBe('none');
    expect(classifyQueueFrame(noticeFrame('boom', 'error'), armedIdle)).toBe('none');
  });

  it('ignores a transient progress notice and a plain info notice', () => {
    expect(classifyQueueFrame(noticeFrame(CANCELLED, 'info', true), ctx({ hasDeferred: true }))).toBe(
      'none',
    );
    expect(classifyQueueFrame(noticeFrame('working…', 'info'), ctx({ hasItems: true }))).toBe(
      'none',
    );
  });
});

describe('canBatchDeferred / hasSendableContent', () => {
  const qi = (id: string, text: string, attachments: QueuedItem['attachments'] = []): QueuedItem => ({
    id,
    text,
    attachments,
  });

  it('batches 2+ plain messages', () => {
    expect(canBatchDeferred([qi('a', 'hi'), qi('b', 'there')])).toBe(true);
  });

  it('sends individually for a lone message', () => {
    expect(canBatchDeferred([qi('a', 'hi')])).toBe(false);
  });

  it('a slash command anywhere in the set is a coalescing barrier', () => {
    expect(canBatchDeferred([qi('a', 'hi'), qi('b', '/stop')])).toBe(false);
  });

  it('filters blanks before applying the 2-item threshold', () => {
    expect(canBatchDeferred([qi('a', 'hi'), qi('b', '  ')])).toBe(false); // only 1 real
    expect(canBatchDeferred([qi('a', 'hi'), qi('b', 'yo'), qi('c', '')])).toBe(true); // 2 real
  });

  it('an attachment-only message counts as real content', () => {
    const withFile = qi('a', '', [
      { kind: 'image', blob_id: 'b1', mime_type: 'image/png', size: 128, filename: 'x.png' },
    ]);
    expect(hasSendableContent(withFile)).toBe(true);
    expect(canBatchDeferred([withFile, qi('b', 'hi')])).toBe(true);
  });
});
