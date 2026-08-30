/// Splitting a STILL-GROWING markdown stream into a settled prefix and a live
/// tail, so the prefix parses once instead of on every tick (the whole-text
/// re-parse is quadratic over a stream — the dominant main-thread cost of a
/// long turn). A boundary is only safe at the TOP LEVEL: splitting inside a
/// fenced code block hands remark an unclosed fence, which swallows everything
/// after it. A boundary the scanner misjudges (a blank line inside a
/// blockquoted fence, a loose list split across chunks) is cosmetic — two
/// adjacent blocks instead of one — and heals when the settled re-render
/// parses the final text whole.

const FENCE_LINE = /^ {0,3}(`{3,}|~{3,})/;

/// The index where the live tail may start: just past the LAST blank line that
/// sits outside any fenced code block or own-line `$$` display-math block, at
/// or after `from`. `from` must itself be a top-level offset — 0, or a value
/// this function previously returned — because the scan starts with all
/// nesting state closed. Returns null when no safe boundary exists past
/// `from`. Only complete lines count: the trailing partial line can still grow
/// into anything.
export function lastSafeBoundary(text: string, from: number): number | null {
  let i = from;
  let fence: { ch: string; len: number } | null = null;
  let math = false;
  let last: number | null = null;
  while (i <= text.length) {
    const nl = text.indexOf("\n", i);
    const line = text.slice(i, nl === -1 ? text.length : nl);
    if (fence !== null) {
      const m = FENCE_LINE.exec(line);
      if (
        m !== null &&
        m[1][0] === fence.ch &&
        m[1].length >= fence.len &&
        line.slice(line.indexOf(m[1]) + m[1].length).trim() === ""
      ) {
        fence = null;
      }
    } else if (math) {
      if (line.trim() === "$$") math = false;
    } else {
      const m = FENCE_LINE.exec(line);
      if (m !== null) fence = { ch: m[1][0], len: m[1].length };
      // Only a bare own-line `$$` opens display math — a one-line `$$x$$` is
      // self-contained and toggles nothing.
      else if (line.trim() === "$$") math = true;
      else if (line.trim() === "" && nl !== -1) last = nl + 1;
    }
    if (nl === -1) break;
    i = nl + 1;
  }
  return last;
}

/// Below this much new settled text, no chunk is cut — bounds the number of
/// chunk elements to roughly text-length / this.
export const STREAM_CHUNK_MIN_CHARS = 1024;

/// Extend `ends` (ascending chunk-end offsets, each a `lastSafeBoundary`
/// result) for the current text. Returns `ends` ITSELF when no new chunk is
/// due, so callers can use identity to skip work. A chunk is cut only when at
/// least STREAM_CHUNK_MIN_CHARS of settled text sit past the previous cut and
/// a non-empty tail remains (the streaming caret rides the tail's last
/// element, so the tail must always have one).
export function advanceChunkEnds(text: string, ends: readonly number[]): readonly number[] {
  const last = ends.length > 0 ? ends[ends.length - 1] : 0;
  const boundary = lastSafeBoundary(text, last);
  if (boundary === null || boundary - last < STREAM_CHUNK_MIN_CHARS || boundary >= text.length) {
    return ends;
  }
  return [...ends, boundary];
}
