import { describe, it, expect } from 'vitest';

import { transcriptTailSignature, type TranscriptRow } from './ChatPage';

// Pins the rule behind the "New messages" pill: it may only fire when content
// lands BELOW the user's viewport. Scroll-up pagination rebuilds the transcript
// array wholesale, so array identity is not that signal — the tail is.

function row(key: string, text = ''): TranscriptRow {
  return { key, role: 'assistant', text };
}

const QUIET = { awaitingReply: false, pendingApproval: false, deferred: 0 };

describe('transcriptTailSignature', () => {
  it('is unchanged by a scroll-up prepend of older pages', () => {
    const thread = [row('a'), row('b')];
    const before = transcriptTailSignature(thread, QUIET);
    const after = transcriptTailSignature([row('old1'), row('old2'), ...thread], QUIET);
    expect(after).toBe(before);
  });

  it('moves when a message is appended', () => {
    expect(transcriptTailSignature([row('a'), row('b')], QUIET)).not.toBe(
      transcriptTailSignature([row('a')], QUIET),
    );
  });

  it('moves as a streaming answer grows', () => {
    expect(transcriptTailSignature([row('a', 'Hello')], QUIET)).not.toBe(
      transcriptTailSignature([row('a', 'Hel')], QUIET),
    );
  });

  it('moves when a step lands in the trailing work block', () => {
    const block: TranscriptRow = { ...row('w'), kind: 'work', steps: [] };
    expect(
      transcriptTailSignature(
        [{ ...block, steps: [{ key: 's1', kind: 'reasoning', text: 'thinking' }] }],
        QUIET,
      ),
    ).not.toBe(transcriptTailSignature([block], QUIET));
  });

  it('moves when the working indicator appears', () => {
    expect(
      transcriptTailSignature([row('a')], { ...QUIET, awaitingReply: true }),
    ).not.toBe(transcriptTailSignature([row('a')], QUIET));
  });

  it('moves when an approval card appears', () => {
    expect(
      transcriptTailSignature([row('a')], { ...QUIET, pendingApproval: true }),
    ).not.toBe(transcriptTailSignature([row('a')], QUIET));
  });

  it('moves when a deferred bubble is parked below the thread', () => {
    expect(transcriptTailSignature([row('a')], { ...QUIET, deferred: 1 })).not.toBe(
      transcriptTailSignature([row('a')], QUIET),
    );
  });

  it('an empty thread has a stable signature', () => {
    expect(transcriptTailSignature([], QUIET)).toBe(transcriptTailSignature([], QUIET));
  });
});
