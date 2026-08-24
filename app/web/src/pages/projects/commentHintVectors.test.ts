import { describe, expect, it } from 'vitest';

import vectors from './commentHintVectors.json';
import { commentHint } from './timelineModel';
import { mentionHint } from './mentionModel';

/**
 * The reference side of the cross-end gate.
 *
 * `commentHintVectors.json` is what the Swift port (`CommentHint.swift`) is
 * held to, and this suite is what stops the file drifting from the
 * implementation that generated it: regenerate without meaning to, or change a
 * rule and forget the regen, and one of the two ends goes red here rather than
 * six weeks later on a phone.
 *
 * The hand-written cases in `timelineModel.test.ts` / `mentionModel.test.ts`
 * remain the place a RULE is argued. This only pins the bytes.
 */
describe('comment hint vectors', () => {
  it('covers every branch worth pinning', () => {
    // A canary: a regen that silently produced a thinner file would otherwise
    // leave both ends passing over almost nothing.
    expect(vectors.comment.length).toBeGreaterThanOrEqual(16);
    expect(vectors.mention.length).toBeGreaterThanOrEqual(10);
    // Both outcomes of the mention hint are represented — a file of all-nulls
    // would assert nothing about the sentence it exists to pin.
    expect(vectors.mention.some((v) => v.hint === null)).toBe(true);
    expect(vectors.mention.some((v) => v.hint !== null)).toBe(true);
  });

  for (const vector of vectors.comment) {
    it(`comment: ${vector.name}`, () => {
      expect(commentHint(vector.issue as never, vector.runs as never, vector.team as never)).toBe(
        vector.hint,
      );
    });
  }

  for (const vector of vectors.mention) {
    it(`mention: ${vector.name}`, () => {
      expect(mentionHint(vector.issue as never, vector.draft, vector.team as never)).toBe(
        vector.hint,
      );
    });
  }
});
