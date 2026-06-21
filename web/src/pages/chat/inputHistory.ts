import { useCallback, useMemo, useRef } from 'react';

// Web-chat composer input history — a faithful port of the TUI input ring
// (`crates/tui/src/app.rs`). The composer is tab-global (one draft shared
// across every conversation, never reset on switch), so the ring is global too
// — Up recalls the last thing you submitted in this browser regardless of which
// conversation you're in, exactly like the single TUI ring. It persists to
// localStorage so it survives a reload the way the TUI ring survives a restart.
// Up walks toward older entries; Down walks back toward the empty draft;
// consecutive duplicates collapse and the ring is capped to the most recent
// `HISTORY_CAP` entries.

export const HISTORY_CAP = 500;
export const HISTORY_KEY = 'aura.inputHistory';

/** Append `line` to the ring, mirroring `App::remember`: trim, drop a
 *  consecutive duplicate of the newest entry, and cap to the most recent
 *  `HISTORY_CAP`. Returns the SAME array reference when nothing changed, so a
 *  caller can skip the persist write. */
export function appendHistory(entries: string[], line: string): string[] {
  const trimmed = line.trim();
  if (trimmed.length === 0) return entries;
  if (entries[entries.length - 1] === trimmed) return entries;
  const next = [...entries, trimmed];
  return next.length > HISTORY_CAP ? next.slice(next.length - HISTORY_CAP) : next;
}

/** Outcome of a navigation step. `text === null` means "no-op, leave the
 *  composer untouched"; any string (including `''`) is the new composer value
 *  and the caller moves the caret to its end. `cursor` is the new ring position
 *  (`null` = not navigating / fresh draft). */
export interface HistoryNav {
  cursor: number | null;
  text: string | null;
}

/** Up / older — mirrors `App::history_prev`. History is only ENTERED from an
 *  empty composer (or while already navigating); from a non-empty fresh draft
 *  it's a no-op so the draft is never clobbered. Repeated steps clamp at the
 *  oldest entry. */
export function historyPrev(
  entries: string[],
  cursor: number | null,
  composerEmpty: boolean,
): HistoryNav {
  if (entries.length === 0) return { cursor, text: null };
  if (cursor === null && !composerEmpty) return { cursor, text: null };
  const next = cursor === null ? entries.length - 1 : Math.max(0, cursor - 1);
  return { cursor: next, text: entries[next] };
}

/** Down / newer — mirrors `App::history_next`. A no-op unless already
 *  navigating; walking past the newest entry drops back to the empty draft
 *  (`text: ''`, `cursor: null`). */
export function historyNext(entries: string[], cursor: number | null): HistoryNav {
  if (cursor === null) return { cursor, text: null };
  if (cursor + 1 >= entries.length) return { cursor: null, text: '' };
  const next = cursor + 1;
  return { cursor: next, text: entries[next] };
}

function loadHistory(): string[] {
  try {
    const raw = window.localStorage.getItem(HISTORY_KEY);
    if (!raw) return [];
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed.filter((e): e is string => typeof e === 'string');
  } catch {
    return [];
  }
}

function saveHistory(entries: string[]): void {
  try {
    window.localStorage.setItem(HISTORY_KEY, JSON.stringify(entries));
  } catch {
    // Best-effort persistence — a full/blocked store just means history is
    // session-only, never a broken composer.
  }
}

export interface InputHistory {
  /** Up: the composer text to show, or `null` to leave the composer alone. */
  recallPrev(composerEmpty: boolean): string | null;
  /** Down: the composer text (possibly `''`), or `null` for a no-op. */
  recallNext(): string | null;
  /** Record a submitted line (call on send). Resets navigation. */
  commit(line: string): void;
  /** Exit history navigation — call when the user edits the composer. */
  reset(): void;
}

/** Stateful wrapper around the pure ring helpers. Entries live in a ref
 *  (loaded once, lazily) and the navigation cursor in another, so walking
 *  history and recording sends never trigger a ChatPage re-render — only the
 *  caller's `setComposer` does. */
export function useInputHistory(): InputHistory {
  const entriesRef = useRef<string[] | null>(null);
  const cursorRef = useRef<number | null>(null);

  // Loaded once, lazily — never on every ChatPage render.
  const entries = useCallback((): string[] => {
    if (entriesRef.current === null) entriesRef.current = loadHistory();
    return entriesRef.current;
  }, []);

  const recallPrev = useCallback(
    (composerEmpty: boolean): string | null => {
      const r = historyPrev(entries(), cursorRef.current, composerEmpty);
      cursorRef.current = r.cursor;
      return r.text;
    },
    [entries],
  );

  const recallNext = useCallback((): string | null => {
    const r = historyNext(entries(), cursorRef.current);
    cursorRef.current = r.cursor;
    return r.text;
  }, [entries]);

  const commit = useCallback(
    (line: string): void => {
      const prev = entries();
      const next = appendHistory(prev, line);
      if (next !== prev) {
        entriesRef.current = next;
        saveHistory(next);
      }
      cursorRef.current = null;
    },
    [entries],
  );

  const reset = useCallback((): void => {
    cursorRef.current = null;
  }, []);

  return useMemo(
    () => ({ recallPrev, recallNext, commit, reset }),
    [recallPrev, recallNext, commit, reset],
  );
}
