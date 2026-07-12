import { describe, expect, it } from "vitest";
import { advanceFromLive, advanceFromSync, type CursorState } from "./cursor";

const CLEAN: CursorState = { cursor: null, rebaseDirty: false };

describe("advanceFromSync", () => {
  it("takes the watermark as the baseline when there is no cursor yet", () => {
    expect(advanceFromSync(CLEAN, 7, false)).toEqual({ cursor: 7, rebaseDirty: false });
  });

  it("is max-wins — a lower watermark never rewinds the cursor", () => {
    const state: CursorState = { cursor: 10, rebaseDirty: false };
    expect(advanceFromSync(state, 4, false)).toEqual({ cursor: 10, rebaseDirty: false });
  });

  it("holds the cursor when the page carries no watermark", () => {
    const state: CursorState = { cursor: 10, rebaseDirty: false };
    expect(advanceFromSync(state, null, false)).toEqual({ cursor: 10, rebaseDirty: false });
  });

  it("advances from a REBASED page's watermark (it is still a sync watermark) and marks it dirty", () => {
    expect(advanceFromSync(CLEAN, 100, true)).toEqual({ cursor: 100, rebaseDirty: true });
  });

  it("clears the dirty flag once a non-rebased sync completes", () => {
    const dirty: CursorState = { cursor: 100, rebaseDirty: true };
    expect(advanceFromSync(dirty, 120, false)).toEqual({ cursor: 120, rebaseDirty: false });
  });

  it("clears the dirty flag even on an empty (no-watermark) non-rebased page", () => {
    const dirty: CursorState = { cursor: 100, rebaseDirty: true };
    expect(advanceFromSync(dirty, null, false)).toEqual({ cursor: 100, rebaseDirty: false });
  });
});

describe("advanceFromLive", () => {
  it("takes a live ordinal as the baseline when there is no cursor yet", () => {
    expect(advanceFromLive(CLEAN, 5)).toEqual({ cursor: 5, rebaseDirty: false });
  });

  it("is max-wins — an older or equal ordinal never moves the cursor", () => {
    const state: CursorState = { cursor: 10, rebaseDirty: false };
    expect(advanceFromLive(state, 9)).toBe(state);
    expect(advanceFromLive(state, 10)).toBe(state);
  });

  it("REFUSES to advance while rebase-dirty", () => {
    const dirty: CursorState = { cursor: 100, rebaseDirty: true };
    expect(advanceFromLive(dirty, 105)).toBe(dirty);
  });

  it("resumes advancing once the follow-up sync cleared the dirty flag", () => {
    const dirty: CursorState = { cursor: 100, rebaseDirty: true };
    const clean = advanceFromSync(dirty, 104, false);
    expect(advanceFromLive(clean, 105)).toEqual({ cursor: 105, rebaseDirty: false });
  });
});

describe("the rebase window", () => {
  // Sync selects strictly `> cursor`. If a live ordinal landing inside the
  // rebase window moved the cursor, the follow-up sync would ask for `> 105`
  // and rows 101..104 — persisted after the rebased page was built — would be
  // skipped by every future sync. Silent, permanent loss.
  it("never lets a live final reply leapfrog rows persisted inside it", () => {
    let state = advanceFromSync(CLEAN, 100, true);
    state = advanceFromLive(state, 105);

    expect(state.cursor).toBe(100);

    const followUp = advanceFromSync(state, 105, false);
    expect(followUp).toEqual({ cursor: 105, rebaseDirty: false });
  });

  it("does not mutate the state it is handed", () => {
    const state: CursorState = { cursor: 3, rebaseDirty: false };
    advanceFromSync(state, 9, true);
    advanceFromLive(state, 9);
    expect(state).toEqual({ cursor: 3, rebaseDirty: false });
  });
});
