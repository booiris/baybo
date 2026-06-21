import { describe, it, expect } from 'vitest';

import { appendHistory, historyNext, historyPrev, HISTORY_CAP } from './inputHistory';

// Pins the composer input ring to the TUI's semantics
// (`crates/tui/src/app.rs` — remember / history_prev / history_next). The pure
// helpers carry all the logic; the React hook is a thin ref wrapper over them.

describe('appendHistory', () => {
  it('trims and records', () => {
    expect(appendHistory([], '  hi  ')).toEqual(['hi']);
  });

  it('rejects empty / whitespace-only lines (same array back)', () => {
    const ring = ['a'];
    expect(appendHistory(ring, '')).toBe(ring);
    expect(appendHistory(ring, '   ')).toBe(ring);
  });

  it('dedupes a consecutive duplicate of the newest entry', () => {
    let ring: string[] = [];
    ring = appendHistory(ring, 'foo');
    ring = appendHistory(ring, 'foo');
    expect(ring).toEqual(['foo']);
  });

  it('keeps a duplicate that is not the most recent entry', () => {
    let ring: string[] = [];
    ring = appendHistory(ring, 'foo');
    ring = appendHistory(ring, 'bar');
    ring = appendHistory(ring, 'foo');
    expect(ring).toEqual(['foo', 'bar', 'foo']);
  });

  it('caps to the most recent HISTORY_CAP entries', () => {
    let ring: string[] = [];
    for (let i = 0; i < HISTORY_CAP + 50; i++) ring = appendHistory(ring, `m${i}`);
    expect(ring).toHaveLength(HISTORY_CAP);
    expect(ring[0]).toBe('m50');
    expect(ring[ring.length - 1]).toBe(`m${HISTORY_CAP + 49}`);
  });
});

describe('historyPrev', () => {
  const ring = ['a', 'b', 'c'];

  it('is inert on an empty ring', () => {
    expect(historyPrev([], null, true)).toEqual({ cursor: null, text: null });
  });

  it('walks back from the empty draft, then clamps at the oldest', () => {
    let nav = historyPrev(ring, null, true);
    expect(nav).toEqual({ cursor: 2, text: 'c' });
    nav = historyPrev(ring, nav.cursor, false);
    expect(nav).toEqual({ cursor: 1, text: 'b' });
    nav = historyPrev(ring, nav.cursor, false);
    nav = historyPrev(ring, nav.cursor, false);
    expect(nav).toEqual({ cursor: 0, text: 'a' });
  });

  it('is inert with a non-empty fresh draft (cursor stays null)', () => {
    expect(historyPrev(ring, null, false)).toEqual({ cursor: null, text: null });
  });

  it('keeps walking once in history mode, ignoring composerEmpty', () => {
    // Already navigating (cursor set): the recalled text is non-empty, yet the
    // next Up must still step back — composerEmpty is irrelevant here.
    expect(historyPrev(ring, 2, false)).toEqual({ cursor: 1, text: 'b' });
  });
});

describe('historyNext', () => {
  const ring = ['a', 'b', 'c'];

  it('is inert when not navigating', () => {
    expect(historyNext(ring, null)).toEqual({ cursor: null, text: null });
  });

  it('walks forward toward newer entries', () => {
    expect(historyNext(ring, 0)).toEqual({ cursor: 1, text: 'b' });
  });

  it('drops back to the empty draft past the newest entry', () => {
    expect(historyNext(ring, 2)).toEqual({ cursor: null, text: '' });
  });
});
