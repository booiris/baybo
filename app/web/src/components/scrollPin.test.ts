import { describe, expect, it } from 'vitest';

import { atBottom, BOTTOM_SLACK_PX } from './scrollPin';

function box(scrollHeight: number, clientHeight: number, scrollTop: number): HTMLElement {
  return { scrollHeight, clientHeight, scrollTop } as HTMLElement;
}

describe('atBottom', () => {
  it('is true at the edge and for a pane too short to scroll', () => {
    expect(atBottom(box(1000, 400, 600))).toBe(true);
    expect(atBottom(box(300, 400, 0))).toBe(true);
  });

  it('holds the reader through the last few pixels, and no further', () => {
    expect(atBottom(box(1000, 400, 600 - BOTTOM_SLACK_PX))).toBe(true);
    expect(atBottom(box(1000, 400, 600 - BOTTOM_SLACK_PX - 1))).toBe(false);
  });

  it('takes a tighter slack where the scroller is smaller', () => {
    // 56px off the bottom: the thread's edge, past a step list's.
    expect(atBottom(box(1000, 400, 544), 48)).toBe(false);
    expect(atBottom(box(1000, 400, 544))).toBe(true);
  });
});
