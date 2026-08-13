import { describe, expect, it } from 'vitest';

import { anchorRowFor, jumpOrdinal } from './searchJump';

/** A rendered thread: one wrapper per row, `null` for a row that carries no
 *  ordinal (a control-event notice, keyed `n<seq>`). */
function thread(rows: (number | null)[]): HTMLElement {
  const scroller = document.createElement('div');
  rows.forEach((ordinal, i) => {
    const row = document.createElement('div');
    row.id = ordinal === null ? `row-s-n${i}` : `row-s-m${ordinal}`;
    if (ordinal !== null) row.dataset.ordinal = String(ordinal);
    scroller.append(row);
  });
  return scroller;
}

describe('anchorRowFor', () => {
  it('lands on the row that carries the ordinal', () => {
    expect(anchorRowFor(thread([2, 7, 9]), 7)?.id).toBe('row-s-m7');
  });

  // The whole reason this is not `getElementById`: a hit on the model's
  // mid-turn prose has no row of its own — the turn's work card swallowed it —
  // so the card that holds it is where the jump belongs.
  it('lands on the row that HOLDS an ordinal with no row of its own', () => {
    expect(anchorRowFor(thread([2, 7, 20]), 12)?.id).toBe('row-s-m7');
  });

  it('ignores rows with no ordinal', () => {
    const scroller = thread([4, null, 30]);
    expect(anchorRowFor(scroller, 12)?.id).toBe('row-s-m4');
  });

  // The walk stopped short (bound hit, or the server has no more): the top of
  // what we loaded is closer than leaving the reader at the tail.
  it('falls back to the oldest loaded row when the target is above it', () => {
    expect(anchorRowFor(thread([40, 51]), 3)?.id).toBe('row-s-m40');
  });

  it('resolves nothing in an empty thread', () => {
    expect(anchorRowFor(thread([]), 3)).toBeNull();
  });
});

describe('jumpOrdinal', () => {
  const hit = (ordinal: number, superseded: number | null) => ({
    ordinal,
    superseded_by: superseded,
    role: 'assistant',
    text: '',
    created_at: '2026-08-13T00:00:00Z',
  });

  it('lands on the hit itself when it is still live', () => {
    expect(jumpOrdinal(hit(12, null))).toBe(12);
  });

  // The matched row is gone from the rendered conversation; the row compaction
  // put in its place is the only thing there is to scroll to.
  it('lands on the replacement when compaction superseded the hit', () => {
    expect(jumpOrdinal(hit(12, 40))).toBe(40);
  });
});
