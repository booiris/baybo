/**
 * The incremental-poll contract for `GET /v1/traces/{session_id}`.
 *
 * Source of truth: `QueryApi::load_trace_overview` (`crates/query/src/lib.rs`)
 * — with `since_ordinal` the server returns ONLY rows with a strictly greater
 * ordinal (so a delta appends, never overlaps) and re-queries the session-wide
 * `supersede_watermark` (MAX of `superseded_by`), which is what tells the
 * client a compaction has re-marked rows inside the prefix it still holds.
 * `turns` is always the full, tiny array — replaced, never merged.
 *
 * The call sequence pinned here is `loadOverview` in
 * `pages/TraceSessionPage.tsx`; child traces use their own page rather than
 * being polled into the parent.
 */
import { describe, expect, it } from 'vitest';
import type { SessionMessageRow, TraceOverview, TraceTurnSummary } from '../../types/trace';
import type { OverviewMerge } from './overviewSync';
import { maxOrdinal, mergeOverviewPage, pollCursor } from './overviewSync';

const T0 = '2026-01-01T00:00:00.000Z';
const T1 = '2026-01-01T00:00:01.000Z';

const SESSION = 'sess-root';
const CHILD = 'sess-child';

function row(ordinal: number): SessionMessageRow {
  return {
    ordinal,
    superseded_by: null,
    created_at: T0,
    message: { role: 'assistant', content: [{ Text: `m${ordinal}` }], source: 'agent' },
  };
}

function turn(turnId: string): TraceTurnSummary {
  return {
    turn_id: turnId,
    session_id: SESSION,
    turn_status_kind: 'completed',
    turn_input_kind: 'user_chat',
    created_at: T0,
    started_at: T0,
    ended_at: T1,
    input_tokens: 0,
    output_tokens: 0,
    cached_input_tokens: 0,
    cache_creation_input_tokens: 0,
  };
}

function mkOverview(
  sessionId: string,
  rows: SessionMessageRow[],
  watermark: number | null,
  turns: TraceTurnSummary[] = [],
): TraceOverview {
  return {
    session_id: sessionId,
    session_messages: rows,
    turns,
    supersede_watermark: watermark,
    external_agent: null,
    subagent_type: null,
  };
}

/** Assert-and-narrow: the `reload` arm carries no overview. */
function overviewOf(merged: OverviewMerge): TraceOverview {
  if (merged.action === 'reload') throw new Error('expected a merged overview, got reload');
  return merged.overview;
}

function ordinals(o: TraceOverview): number[] {
  return o.session_messages.map((r) => r.ordinal);
}

describe('maxOrdinal', () => {
  it('is undefined for an empty page — a cold start has no cursor', () => {
    expect(maxOrdinal([])).toBeUndefined();
  });

  it('returns the single row ordinal', () => {
    expect(maxOrdinal([row(7)])).toBe(7);
  });

  it('finds the true max even when rows are NOT sorted ascending', () => {
    expect(maxOrdinal([row(3), row(9), row(1), row(4)])).toBe(9);
  });

  it('finds the max when the largest row comes first', () => {
    expect(maxOrdinal([row(12), row(2), row(11)])).toBe(12);
  });

  it('returns 0 rather than undefined when the highest ordinal is 0', () => {
    expect(maxOrdinal([row(0)])).toBe(0);
  });
});

describe('pollCursor', () => {
  it('is undefined for a null held overview (cold start pulls the full transcript)', () => {
    expect(pollCursor(null, SESSION)).toBeUndefined();
  });

  it('is undefined for an undefined held overview (child not fetched yet)', () => {
    expect(pollCursor(undefined, CHILD)).toBeUndefined();
  });

  it('is undefined when the held overview belongs to ANOTHER session', () => {
    const held = mkOverview(SESSION, [row(1), row(2), row(3)], null);
    expect(pollCursor(held, CHILD)).toBeUndefined();
  });

  it('is undefined when the held overview has no messages yet', () => {
    expect(pollCursor(mkOverview(SESSION, [], null), SESSION)).toBeUndefined();
  });

  it('is the max held ordinal for the same session', () => {
    const held = mkOverview(SESSION, [row(1), row(5), row(2)], 4);
    expect(pollCursor(held, SESSION)).toBe(5);
  });

  it('is 0 — not undefined — when the only held row is ordinal 0', () => {
    expect(pollCursor(mkOverview(SESSION, [row(0)], null), SESSION)).toBe(0);
  });
});

describe('mergeOverviewPage — replace', () => {
  it("a cold start (sinceOrdinal undefined) replaces and hands back the fresh page as-is", () => {
    const fresh = mkOverview(SESSION, [row(1), row(2)], null, [turn('t1')]);
    const merged = mergeOverviewPage(null, fresh, undefined);

    expect(merged.action).toBe('replace');
    expect(overviewOf(merged)).toBe(fresh);
  });

  it('replaces even when an overview IS held, if no cursor was sent (the page is full, not a delta)', () => {
    const held = mkOverview(SESSION, [row(1), row(2)], null);
    const fresh = mkOverview(SESSION, [row(1), row(2), row(3)], null);
    const merged = mergeOverviewPage(held, fresh, undefined);

    expect(merged.action).toBe('replace');
    expect(overviewOf(merged)).toBe(fresh);
    expect(ordinals(overviewOf(merged))).toEqual([1, 2, 3]);
  });

  it('replaces when a cursor was sent but nothing is held (defensive branch)', () => {
    const fresh = mkOverview(SESSION, [row(4), row(5)], null);
    const merged = mergeOverviewPage(null, fresh, 3);

    expect(merged.action).toBe('replace');
    expect(overviewOf(merged)).toBe(fresh);
  });

  it('replaces when a cursor was sent and held is undefined (defensive branch)', () => {
    const fresh = mkOverview(CHILD, [row(1)], null);
    const merged = mergeOverviewPage(undefined, fresh, 0);

    expect(merged.action).toBe('replace');
    expect(overviewOf(merged)).toBe(fresh);
  });
});

describe('mergeOverviewPage — append', () => {
  it('concatenates held rows BEFORE the delta rows, in that order', () => {
    const held = mkOverview(SESSION, [row(1), row(2), row(3)], null);
    const fresh = mkOverview(SESSION, [row(4), row(5)], null);
    const merged = mergeOverviewPage(held, fresh, 3);

    expect(merged.action).toBe('append');
    expect(ordinals(overviewOf(merged))).toEqual([1, 2, 3, 4, 5]);
  });

  it('appends when both watermarks are null (no compaction has ever run)', () => {
    const held = mkOverview(SESSION, [row(1)], null);
    const fresh = mkOverview(SESSION, [row(2)], null);

    expect(mergeOverviewPage(held, fresh, 1).action).toBe('append');
  });

  it('appends when the watermark is unchanged at the same non-null value', () => {
    const held = mkOverview(SESSION, [row(1), row(2)], 2);
    const fresh = mkOverview(SESSION, [row(3)], 2);
    const merged = mergeOverviewPage(held, fresh, 2);

    expect(merged.action).toBe('append');
    expect(ordinals(overviewOf(merged))).toEqual([1, 2, 3]);
    expect(overviewOf(merged).supersede_watermark).toBe(2);
  });

  it('keeps the FRESH turns array and watermark, not the held ones', () => {
    const heldTurns = [turn('t1')];
    const freshTurns = [turn('t1'), turn('t2')];
    const held = mkOverview(SESSION, [row(1)], 1, heldTurns);
    const fresh = mkOverview(SESSION, [row(2)], 1, freshTurns);
    const merged = mergeOverviewPage(held, fresh, 1);
    const out = overviewOf(merged);

    expect(merged.action).toBe('append');
    expect(out.turns).toBe(freshTurns);
    expect(out.turns.map((t) => t.turn_id)).toEqual(['t1', 't2']);
    expect(out.supersede_watermark).toBe(1);
  });

  it('keeps every other fresh field (session id, external agent, subagent type)', () => {
    const held: TraceOverview = {
      ...mkOverview(CHILD, [row(1)], null),
      external_agent: 'claude',
      subagent_type: 'researcher',
    };
    const fresh: TraceOverview = {
      ...mkOverview(CHILD, [row(2)], null),
      external_agent: 'codex',
      subagent_type: 'reviewer',
    };
    const out = overviewOf(mergeOverviewPage(held, fresh, 1));

    expect(out.session_id).toBe(CHILD);
    expect(out.external_agent).toBe('codex');
    expect(out.subagent_type).toBe('reviewer');
  });

  it('an empty delta leaves the held transcript intact', () => {
    const held = mkOverview(SESSION, [row(1), row(2)], null);
    const fresh = mkOverview(SESSION, [], null, [turn('t1')]);
    const merged = mergeOverviewPage(held, fresh, 2);

    expect(merged.action).toBe('append');
    expect(ordinals(overviewOf(merged))).toEqual([1, 2]);
    expect(overviewOf(merged).turns.map((t) => t.turn_id)).toEqual(['t1']);
  });

  it('does not mutate the held or fresh arrays', () => {
    const heldRows = [row(1)];
    const freshRows = [row(2)];
    const held = mkOverview(SESSION, heldRows, null);
    const fresh = mkOverview(SESSION, freshRows, null);
    const out = overviewOf(mergeOverviewPage(held, fresh, 1));

    expect(heldRows.map((r) => r.ordinal)).toEqual([1]);
    expect(freshRows.map((r) => r.ordinal)).toEqual([2]);
    expect(out.session_messages).not.toBe(heldRows);
    expect(out.session_messages).not.toBe(freshRows);
  });

  it('appends off a cursor of 0 (the falsy cursor is still a real cursor)', () => {
    const held = mkOverview(SESSION, [row(0)], null);
    const fresh = mkOverview(SESSION, [row(1)], null);
    const merged = mergeOverviewPage(held, fresh, 0);

    expect(merged.action).toBe('append');
    expect(ordinals(overviewOf(merged))).toEqual([0, 1]);
  });
});

describe('mergeOverviewPage — reload', () => {
  it('reloads when the watermark moves null -> number (the first compaction)', () => {
    const held = mkOverview(SESSION, [row(1), row(2)], null);
    const fresh = mkOverview(SESSION, [row(3)], 3);

    expect(mergeOverviewPage(held, fresh, 2)).toEqual({ action: 'reload' });
  });

  it('reloads when the watermark moves to a DIFFERENT number (a later compaction)', () => {
    const held = mkOverview(SESSION, [row(1), row(2), row(3)], 3);
    const fresh = mkOverview(SESSION, [row(4)], 7);

    expect(mergeOverviewPage(held, fresh, 3)).toEqual({ action: 'reload' });
  });

  it('reloads when the watermark moves number -> null', () => {
    const held = mkOverview(SESSION, [row(1)], 1);
    const fresh = mkOverview(SESSION, [row(2)], null);

    expect(mergeOverviewPage(held, fresh, 1)).toEqual({ action: 'reload' });
  });

  it('reloads on a moved watermark even when the delta page is empty', () => {
    const held = mkOverview(SESSION, [row(1), row(2)], 1);
    const fresh = mkOverview(SESSION, [], 2);

    expect(mergeOverviewPage(held, fresh, 2)).toEqual({ action: 'reload' });
  });

  it('discards the delta rows — reload carries no overview to render', () => {
    const held = mkOverview(SESSION, [row(1)], null);
    const fresh = mkOverview(SESSION, [row(2)], 2);
    const merged = mergeOverviewPage(held, fresh, 1);

    expect(merged.action).toBe('reload');
    expect(Object.keys(merged)).toEqual(['action']);
  });
});

describe('real poll sequences', () => {
  it('loadOverview: cold start, then an incremental poll that appends', () => {
    // Tick 1 — nothing held, so the full transcript comes back and replaces.
    const cold = pollCursor(null, SESSION);
    expect(cold).toBeUndefined();
    const full = mkOverview(SESSION, [row(1), row(2)], null, [turn('t1')]);
    let heldOverview = overviewOf(mergeOverviewPage(null, full, cold));
    expect(ordinals(heldOverview)).toEqual([1, 2]);

    // Tick 2 — poll above the held cursor; the delta appends.
    const since = pollCursor(heldOverview, SESSION);
    expect(since).toBe(2);
    const delta = mkOverview(SESSION, [row(3), row(4)], null, [turn('t1'), turn('t2')]);
    heldOverview = overviewOf(mergeOverviewPage(heldOverview, delta, since));
    expect(ordinals(heldOverview)).toEqual([1, 2, 3, 4]);
    expect(pollCursor(heldOverview, SESSION)).toBe(4);
  });

  it('loadOverview: a compaction forces reload, and the refetched full page replaces the prefix', () => {
    const held = mkOverview(SESSION, [row(1), row(2), row(3)], null);
    const since = pollCursor(held, SESSION);
    expect(since).toBe(3);

    const delta = mkOverview(SESSION, [row(4)], 4);
    expect(mergeOverviewPage(held, delta, since).action).toBe('reload');

    // The caller refetches with no cursor; that page replaces wholesale.
    const refetched = mkOverview(SESSION, [row(4), row(5)], 4);
    const after = overviewOf(mergeOverviewPage(held, refetched, undefined));
    expect(ordinals(after)).toEqual([4, 5]);
    expect(after.supersede_watermark).toBe(4);
  });

  it('fetchChildOverview: a child keyed by its own id never inherits the parent cursor', () => {
    const parent = mkOverview(SESSION, [row(1), row(2), row(3)], null);

    // The child map has no entry yet — cold start, full transcript.
    expect(pollCursor(undefined, CHILD)).toBeUndefined();

    // And a mis-keyed hand-off (the parent's overview) is refused outright,
    // so no parent ordinal is ever sent as the child's `since_ordinal`.
    expect(pollCursor(parent, CHILD)).toBeUndefined();

    const childFull = mkOverview(CHILD, [row(1)], null);
    const childHeld = overviewOf(mergeOverviewPage(undefined, childFull, pollCursor(undefined, CHILD)));
    expect(pollCursor(childHeld, CHILD)).toBe(1);
  });

  it('fetchChildOverview: a live external child streams rows in over successive polls', () => {
    let held = mkOverview(CHILD, [], null);
    // First poll: the child exists but has flushed nothing — cold every tick
    // until the first row lands.
    expect(pollCursor(held, CHILD)).toBeUndefined();
    held = overviewOf(mergeOverviewPage(held, mkOverview(CHILD, [row(1)], null), undefined));

    for (const next of [2, 3, 4]) {
      const since = pollCursor(held, CHILD);
      expect(since).toBe(next - 1);
      held = overviewOf(mergeOverviewPage(held, mkOverview(CHILD, [row(next)], null), since));
    }
    expect(ordinals(held)).toEqual([1, 2, 3, 4]);
  });
});
