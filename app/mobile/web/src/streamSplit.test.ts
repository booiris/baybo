import { describe, expect, it } from "vitest";
import { advanceChunkEnds, lastSafeBoundary, STREAM_CHUNK_MIN_CHARS } from "./streamSplit";

describe("lastSafeBoundary", () => {
  it("lands after the last top-level blank line", () => {
    const text = "one\n\ntwo\n\nthree is still growing";
    expect(lastSafeBoundary(text, 0)).toBe(text.indexOf("three"));
  });

  it("ignores blank lines inside a fenced code block", () => {
    const text = "intro\n\n```rust\nlet a = 1;\n\nlet b = 2;\n";
    // The only safe boundary is the one before the fence opened.
    expect(lastSafeBoundary(text, 0)).toBe(text.indexOf("```rust"));
  });

  it("sees the fence close and offers boundaries after it", () => {
    const text = "```\ncode\n\n```\n\nafter";
    expect(lastSafeBoundary(text, 0)).toBe(text.indexOf("after"));
  });

  it("requires the closing fence to match char and length", () => {
    const tildeClosedByBackticks = "~~~\ncode\n```\n\ntail";
    expect(lastSafeBoundary(tildeClosedByBackticks, 0)).toBeNull();
    const longOpenShortClose = "`````\ncode\n```\n\ntail";
    expect(lastSafeBoundary(longOpenShortClose, 0)).toBeNull();
    const longerCloseIsFine = "```\ncode\n`````\n\ntail";
    expect(lastSafeBoundary(longerCloseIsFine, 0)).toBe(longerCloseIsFine.indexOf("tail"));
  });

  it("ignores blank lines inside an own-line $$ display block", () => {
    const text = "before\n\n$$\na + b\n\nc + d\n";
    expect(lastSafeBoundary(text, 0)).toBe(text.indexOf("$$"));
  });

  it("does not treat a one-line $$x$$ as opening display math", () => {
    const text = "$$E = mc^2$$\n\ntail";
    expect(lastSafeBoundary(text, 0)).toBe(text.indexOf("tail"));
  });

  it("never counts the trailing partial line", () => {
    // The final "\n\n" has no complete blank LINE yet — the second newline
    // ends the blank line, and nothing complete follows; the earlier boundary
    // still stands.
    expect(lastSafeBoundary("one\n\ntwo", 0)).toBe(5);
    expect(lastSafeBoundary("one", 0)).toBeNull();
  });

  it("scans only from the given offset", () => {
    const text = "a\n\nb\n\nc grows";
    const from = text.indexOf("b");
    expect(lastSafeBoundary(text, from)).toBe(text.indexOf("c grows"));
  });
});

describe("advanceChunkEnds", () => {
  const para = (s: string) => `${s.repeat(200)}\n\n`; // ≈ 200·len + 2 chars

  it("returns the same array (identity) when too little has settled", () => {
    const ends: readonly number[] = [];
    expect(advanceChunkEnds("short\n\ntail", ends)).toBe(ends);
  });

  it("cuts a chunk once enough settled text sits before a boundary", () => {
    const text = para("abcdefgh") + "tail grows";
    const out = advanceChunkEnds(text, []);
    expect(out).toHaveLength(1);
    expect(out[0]).toBe(text.indexOf("tail"));
    expect(out[0]).toBeGreaterThanOrEqual(STREAM_CHUNK_MIN_CHARS);
  });

  it("refuses a cut that would leave an empty tail", () => {
    const text = para("abcdefgh");
    const ends: readonly number[] = [];
    expect(advanceChunkEnds(text, ends)).toBe(ends);
  });

  it("extends incrementally as the stream grows", () => {
    const first = para("abcdefgh") + "middle";
    const one = advanceChunkEnds(first, []);
    expect(one).toHaveLength(1);
    const grown = para("abcdefgh") + para("ijklmnop") + "tail";
    const two = advanceChunkEnds(grown, one);
    expect(two).toHaveLength(2);
    expect(grown.slice(two[0], two[1])).toContain("ijklmnop");
  });
});
