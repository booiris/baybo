import { describe, expect, it } from 'vitest';
import type { ContextPart, ContextSegment } from '../../types/trace';
import { buildContextGrid, largestSegments } from './contextGrid';

function seg(part: ContextPart, tokens: number, index = 0): ContextSegment {
  return { part, label: part, tokens, index };
}

const cellsOf = (grid: ReturnType<typeof buildContextGrid>) =>
  grid.categories.reduce((n, c) => n + c.cells, 0) + grid.freeCells;

describe('buildContextGrid', () => {
  const segments = [
    seg('tools', 9_000),
    seg('system_prompt', 6_000),
    seg('tool_result', 15_000),
    seg('user', 100),
  ];

  it('fills the whole grid: parts plus free space equal the cell count', () => {
    // The free space is whatever is left over, so any drift in the part
    // allocation shows up as a visibly wrong empty area.
    const grid = buildContextGrid(segments, 30_100, 200_000);
    expect(cellsOf(grid)).toBe(grid.totalCells);
  });

  it('scales the estimated split onto the number the provider billed', () => {
    // The split is a tiktoken estimate and the total is not. Reporting the
    // estimate as the total would put a number on screen that disagrees with
    // the span's own metadata tab.
    const grid = buildContextGrid(segments, 60_200, null);
    const total = grid.categories.reduce((n, c) => n + c.tokens, 0);
    expect(total).toBeGreaterThanOrEqual(60_198);
    expect(total).toBeLessThanOrEqual(60_202);
    // Proportions survive the scaling: tool results were half the estimate.
    const toolResults = grid.categories.find((c) => c.part === 'tool_result');
    expect(toolResults?.tokens).toBeGreaterThan(29_000);
    expect(toolResults?.tokens).toBeLessThan(31_000);
  });

  it('never rounds a real contributor down to no cells', () => {
    // A 0.3% part is exactly the kind of thing someone opens this panel to
    // find. Independent rounding drops it; largest-remainder with a floor of
    // one cell keeps it drawable.
    const grid = buildContextGrid(segments, 30_100, 200_000);
    const user = grid.categories.find((c) => c.part === 'user');
    expect(user?.cells).toBeGreaterThanOrEqual(1);
  });

  it('gives a spare cell to the larger part, not to whichever came first', () => {
    const grid = buildContextGrid([seg('tools', 700), seg('user', 300)], 1_000, null);
    const tools = grid.categories.find((c) => c.part === 'tools')!;
    const user = grid.categories.find((c) => c.part === 'user')!;
    expect(tools.cells).toBeGreaterThan(user.cells);
  });

  it('shows no free space when the model has no known window', () => {
    // Inventing a window would make an unremarkable call look nearly empty.
    const grid = buildContextGrid(segments, 30_100, null);
    expect(grid.freeCells).toBe(0);
    expect(grid.freeTokens).toBeNull();
    expect(grid.scale).toBe(30_100);
  });

  it('sizes the grid to the window but still measures share against the input', () => {
    // The window is what the CELLS span — that is how free space becomes
    // visible. The percentages stay a share of what was sent, so the legend
    // and the per-item list share one denominator.
    const grid = buildContextGrid([seg('tools', 100)], 50_000, 200_000);
    expect(grid.scale).toBe(200_000);
    expect(grid.freeTokens).toBe(150_000);
    expect(grid.categories[0].share).toBeCloseTo(1, 5);
    expect(grid.freeCells).toBeGreaterThan(grid.categories[0].cells);
  });

  it('falls back to the used total when a call somehow overflowed its window', () => {
    // A window that is smaller than what was sent is a config mismatch, not a
    // reason to draw negative free space.
    const grid = buildContextGrid([seg('tools', 100)], 250_000, 200_000);
    expect(grid.scale).toBe(250_000);
    expect(grid.freeTokens).toBe(0);
  });

  it('rounds the cell size to something a caption can say out loud', () => {
    const grid = buildContextGrid([seg('tools', 100)], 200_000, 200_000);
    expect([250, 500, 1_000]).toContain(grid.tokensPerCell);
  });

  it('draws nothing rather than dividing by zero on an empty context', () => {
    const grid = buildContextGrid([], 0, null);
    expect(grid.categories).toEqual([]);
    expect(grid.totalCells).toBe(0);
    expect(grid.freeCells).toBe(0);
  });

  it('still lays out a context whose usage was never reported', () => {
    // A cancelled or failed call has no `input_tokens`; the caller passes the
    // estimate instead and the grid must behave normally.
    const grid = buildContextGrid(segments, 30_100, null);
    expect(grid.categories.length).toBe(4);
    expect(cellsOf(grid)).toBe(grid.totalCells);
  });
});

describe('largestSegments', () => {
  const scaled = (part: ContextPart, tokens: number, index: number) => ({
    ...seg(part, tokens, index),
    share: 0,
  });

  it('ranks individual pieces, not their categories', () => {
    // "one 15k bash result" and "tool results total 15k" are different
    // findings, and only the first tells you what to delete.
    const ranked = largestSegments(
      [scaled('tool_result', 5_000, 1), scaled('tool_result', 15_000, 2), scaled('user', 100, 3)],
      2,
    );
    expect(ranked.map((s) => s.tokens)).toEqual([15_000, 5_000]);
  });

  it('does not mutate its input', () => {
    const segments = [scaled('user', 1, 0), scaled('tools', 9, 1)];
    largestSegments(segments, 2);
    expect(segments.map((s) => s.part)).toEqual(['user', 'tools']);
  });
});

describe('the legend and the per-item list agree', () => {
  // Both used to scale independently — the legend onto the billed total, the
  // item list not at all — so one tool set printed as two different numbers a
  // few percent apart, in the same panel.
  const segments = [
    seg('tools', 9_000, 0),
    seg('tool_result', 15_000, 1),
    seg('tool_result', 3_000, 2),
    seg('user', 100, 3),
  ];
  const grid = buildContextGrid(segments, 30_100, 200_000);

  it('a category equals the sum of its own segments', () => {
    for (const category of grid.categories) {
      const fromSegments = grid.segments
        .filter((s) => s.part === category.part)
        .reduce((n, s) => n + s.tokens, 0);
      // Both sides round independently, so allow the rounding itself.
      expect(Math.abs(fromSegments - category.tokens)).toBeLessThanOrEqual(
        grid.segments.filter((s) => s.part === category.part).length,
      );
    }
  });

  it('scales the segments onto the billed total too, not just the categories', () => {
    const tools = grid.segments.find((s) => s.part === 'tools')!;
    expect(tools.tokens).not.toBe(9_000);
    expect(tools.tokens).toBe(Math.round((9_000 / 27_100) * 30_100));
  });

  it('measures every share against what was sent, never the window', () => {
    // A part that is 33% of the input and 5% of the window has to pick one
    // number, or the two lists cannot be read against each other.
    const total = grid.categories.reduce((n, c) => n + c.share, 0);
    expect(total).toBeCloseTo(1, 5);
    const tools = grid.categories.find((c) => c.part === 'tools')!;
    const toolsSegment = grid.segments.find((s) => s.part === 'tools')!;
    expect(toolsSegment.share).toBeCloseTo(tools.share, 5);
  });
});
