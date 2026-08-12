/**
 * Pure layout for the context matrix: turn one LLM call's per-part token
 * split into a fixed grid of cells.
 *
 * Kept out of the component so the arithmetic that has to be exactly right —
 * the cells sum to the grid, the parts sum to the total, a non-zero part never
 * rounds away to nothing — is unit-testable without rendering anything.
 */
import type { ContextPart, ContextSegment } from '../../types/trace';

/** Roughly how many cells the grid should hold. The real count is rounded to
 *  whole rows by the component; this only sets the resolution. */
const TARGET_CELLS = 480;

/** Cell sizes to snap to, so the "≈ N tokens per cell" caption reads as a
 *  round number instead of `≈ 437 tokens`. */
const NICE_STEPS = [1, 2, 5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000];

export interface ContextCategory {
  part: ContextPart;
  /** Tokens across every segment of this part, scaled onto the billed total. */
  tokens: number;
  /** Share of what was sent. Every share in the panel uses this one
   *  denominator — the window appears in the headline and as free cells, not
   *  as a second percentage a reader has to reconcile against the first. */
  share: number;
  cells: number;
}

/** A segment with the same scaling its category got, so a per-item list and
 *  the legend can never print two different numbers for one thing. */
export interface ScaledSegment extends ContextSegment {
  share: number;
}

export interface ContextGrid {
  categories: ContextCategory[];
  /** Every segment, scaled — the numbers the categories are summed from. */
  segments: ScaledSegment[];
  /** Cells left unfilled — the model's remaining window. Zero when no window
   *  is known, in which case the grid shows only what was sent. */
  freeCells: number;
  /** Tokens those free cells stand for. `null` when no window is known. */
  freeTokens: number | null;
  tokensPerCell: number;
  totalCells: number;
  /** Denominator every `share` is against: the window, or the used total. */
  scale: number;
}

/** Snap up to the next "round" cell size at or above `raw`. */
function niceCellSize(raw: number): number {
  for (const step of NICE_STEPS) {
    if (step >= raw) return step;
  }
  // Beyond the table, keep it round to the nearest 25k rather than exact.
  return Math.ceil(raw / 25_000) * 25_000;
}

/**
 * Lay the split out as cells.
 *
 * `usedTokens` is the number to trust for the total — the provider's reported
 * `input_tokens` when there is one. The segments only supply proportions:
 * they are a tiktoken estimate and their sum drifts from what was billed, so
 * scaling them onto the reported total keeps the grid's headline exact and
 * confines the estimate to the split.
 *
 * `contextWindow` is optional because the trace outlives the config — a span
 * can name a model no configured client serves any more. Without it the grid
 * is the input alone and shows no free space, rather than inventing a window.
 */
export function buildContextGrid(
  segments: ContextSegment[],
  usedTokens: number,
  contextWindow: number | null,
): ContextGrid {
  const estimated = segments.reduce((n, s) => n + s.tokens, 0);
  const used = Math.max(0, usedTokens);
  const scale = contextWindow != null && contextWindow > used ? contextWindow : used;

  // ONE scaling factor, applied to segments and categories alike. Scaling in
  // two places is how the legend and the per-item list came to print two
  // different numbers for the same tool set.
  const factor = estimated > 0 ? used / estimated : 0;
  const scaledSegments: ScaledSegment[] = segments.map((s) => ({
    ...s,
    tokens: Math.round(s.tokens * factor),
    share: used > 0 ? (s.tokens * factor) / used : 0,
  }));

  const byPart = new Map<ContextPart, number>();
  for (const segment of segments) {
    byPart.set(segment.part, (byPart.get(segment.part) ?? 0) + segment.tokens);
  }
  const scaled: { part: ContextPart; tokens: number }[] = [...byPart.entries()].map(
    ([part, tokens]) => ({ part, tokens: tokens * factor }),
  );
  scaled.sort((a, b) => b.tokens - a.tokens);

  if (scale <= 0) {
    return {
      categories: [],
      segments: scaledSegments,
      freeCells: 0,
      freeTokens: contextWindow != null ? contextWindow : null,
      tokensPerCell: 1,
      totalCells: 0,
      scale: 0,
    };
  }

  const tokensPerCell = niceCellSize(Math.max(1, Math.ceil(scale / TARGET_CELLS)));
  const totalCells = Math.max(1, Math.ceil(scale / tokensPerCell));
  const usedCells = Math.min(totalCells, Math.round(used / tokensPerCell));

  const categories = allocateCells(scaled, usedCells).map(({ part, tokens, cells }) => ({
    part,
    tokens: Math.round(tokens),
    share: used > 0 ? tokens / used : 0,
    cells,
  }));

  const spent = categories.reduce((n, c) => n + c.cells, 0);
  return {
    categories,
    segments: scaledSegments,
    freeCells: Math.max(0, totalCells - spent),
    freeTokens: contextWindow != null ? Math.max(0, contextWindow - used) : null,
    tokensPerCell,
    totalCells,
    scale,
  };
}

/**
 * Hand out `budget` cells across weighted parts by largest remainder.
 *
 * Rounding each part independently would let the row count drift off the
 * grid — visibly, since the free space is whatever is left over. Largest
 * remainder makes the cells sum to exactly `budget`. A part with any tokens
 * at all is floored at one cell first: rounding a real 0.4% contributor down
 * to nothing is how a context grid quietly stops mentioning the thing someone
 * came to find.
 */
function allocateCells(
  parts: { part: ContextPart; tokens: number }[],
  budget: number,
): { part: ContextPart; tokens: number; cells: number }[] {
  const present = parts.filter((p) => p.tokens > 0);
  if (present.length === 0 || budget <= 0) {
    return parts.map((p) => ({ ...p, cells: 0 }));
  }
  // Not every part can be floored at one cell when the budget is smaller than
  // the number of parts; the largest ones win the cells that exist.
  const guaranteed = Math.min(present.length, budget);
  const total = present.reduce((n, p) => n + p.tokens, 0);
  const remaining = budget - guaranteed;

  const exact = present.map((p, i) => ({
    ...p,
    base: i < guaranteed ? 1 : 0,
    want: (p.tokens / total) * remaining,
  }));
  const withFloor = exact.map((p) => ({ ...p, cells: p.base + Math.floor(p.want) }));
  let left = budget - withFloor.reduce((n, p) => n + p.cells, 0);

  // Ties broken by the input order, which is descending by tokens — so a
  // spare cell goes to the larger part, never at random.
  const order = withFloor
    .map((p, i) => ({ i, frac: p.want - Math.floor(p.want) }))
    .sort((a, b) => (b.frac !== a.frac ? b.frac - a.frac : a.i - b.i));
  for (const { i } of order) {
    if (left <= 0) break;
    withFloor[i].cells += 1;
    left -= 1;
  }

  const allocated = new Map(withFloor.map((p) => [p.part, p.cells]));
  return parts.map((p) => ({ ...p, cells: allocated.get(p.part) ?? 0 }));
}

/** The largest individual contributors, for the "what is actually big" list.
 *  Segments, not categories: "one 40k bash result" is a different finding
 *  from "tool results total 60k".
 *
 *  Takes the grid's already-scaled segments, so these numbers are the same
 *  ones the legend adds up. */
export function largestSegments(segments: ScaledSegment[], limit: number): ScaledSegment[] {
  return [...segments].sort((a, b) => b.tokens - a.tokens).slice(0, limit);
}
