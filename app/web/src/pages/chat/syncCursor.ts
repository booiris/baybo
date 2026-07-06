// Per-session sync cursor (protocol v2). One cursor per session, advanced
// max-wins from the sync response's coverage watermark (`next_cursor`) and
// from ordinal-stamped live Message frames — EXCEPT while rebase-dirty,
// when only a sync `next_cursor` may advance it (a live final-reply ordinal
// landing between the rebased page's build and the follow-up sync would
// otherwise leapfrog rows persisted in that window forever, since sync
// selects strictly `> cursor`). One non-rebased sync completing clears the
// dirty flag. See docs/sync-protocol.md "Rebase-dirty cursor".

export interface CursorState {
  /** Highest coverage watermark known for the session; `null` = no
   *  baseline yet (the next sync omits `since_ordinal` and REPLACEs). */
  cursor: number | null;
  /** True after applying a rebased page, until one non-rebased sync
   *  completes. Live ordinals render but do not advance the cursor. */
  rebaseDirty: boolean;
}

export const INITIAL_CURSOR: CursorState = Object.freeze({
  cursor: null,
  rebaseDirty: false,
});

/** Advance from a completed sync. `next_cursor` always feeds the max —
 *  even on a rebased page (it is a sync watermark) — and the dirty flag
 *  tracks whether THIS response was rebased: set on rebase, cleared by
 *  any non-rebased sync. */
export function advanceFromSync(
  state: CursorState,
  nextCursor: number | null,
  rebased: boolean,
): CursorState {
  const cursor =
    nextCursor === null
      ? state.cursor
      : state.cursor === null
        ? nextCursor
        : Math.max(state.cursor, nextCursor);
  return { cursor, rebaseDirty: rebased };
}

/** Advance from an ordinal-stamped live Message frame. Max-wins, but a
 *  rebase-dirty cursor is frozen against live advances. */
export function advanceFromLive(state: CursorState, ordinal: number): CursorState {
  if (state.rebaseDirty) return state;
  if (state.cursor !== null && state.cursor >= ordinal) return state;
  return { cursor: ordinal, rebaseDirty: false };
}
