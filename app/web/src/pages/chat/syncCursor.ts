// Ported verbatim from `app/ios/web/src/transcript/cursor.ts` (a separate pnpm
// workspace root, so the module can't simply be shared). Two clients run the
// one sync loop; a cursor rule that holds on only one of them is the same
// silent, permanent data loss it exists to prevent — `diff` the pair before
// changing either. `mathDelimiters.port.test.ts` enforces it.
//
// The per-session sync cursor (protocol v2): one cursor per session, advanced
// max-wins from a sync response's coverage watermark (`next_cursor`) and from
// ordinal-stamped live final replies — EXCEPT while rebase-dirty, when only a
// sync `next_cursor` may advance it. Sync selects strictly `> cursor`, so a
// live ordinal landing between a rebased page's build and its follow-up sync
// would otherwise leapfrog every row persisted in that window, forever. One
// non-rebased sync clears the flag.
// See docs/sync-protocol.md "Rebase-dirty cursor".

export type CursorState = {
  /// Highest coverage watermark held for the session. `null` = no baseline yet
  /// (the next sync omits `since_ordinal` and REPLACEs on the newest page) —
  /// never a sentinel number.
  cursor: number | null;
  /// True after applying a rebased page, until one non-rebased sync completes.
  /// Live ordinals still render; they just don't move the cursor.
  rebaseDirty: boolean;
};

/// The no-baseline-yet state, frozen because it is the shared default handed
/// out for every session that has not synced yet.
export const INITIAL_CURSOR: CursorState = Object.freeze({
  cursor: null,
  rebaseDirty: false,
});

/// Advance from a completed sync. `next_cursor` always feeds the max — even on
/// a rebased page, since it is the sync's own watermark — and the dirty flag
/// tracks whether THIS response was rebased: set on rebase, cleared by any
/// non-rebased sync.
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

/// Advance from an ordinal-stamped live final reply. Max-wins, but a
/// rebase-dirty cursor is frozen against live advances.
export function advanceFromLive(state: CursorState, ordinal: number): CursorState {
  if (state.rebaseDirty) return state;
  if (state.cursor !== null && state.cursor >= ordinal) return state;
  return { cursor: ordinal, rebaseDirty: false };
}
