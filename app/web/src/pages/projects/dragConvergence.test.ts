import { describe, expect, it } from 'vitest';
import { pointerWithin, rectIntersection } from '@dnd-kit/core';
import type { Active, ClientRect, CollisionDetection, DroppableContainer } from '@dnd-kit/core';

import {
  COLUMNS,
  cardDragId,
  columnDropId,
  groupByStatus,
  moveCard,
  resolveDrop,
  type Board,
  type Issue,
  type IssueStatus,
} from './boardModel';
import { EMPTY_FILTER, filterBoard, type BoardFilter } from './boardFilter';
import { boardCollisionDetection } from './dropTarget';

/// One live drag, simulated to a fixed point, with the cursor held still.
///
/// The board previews a move by **mutating its own state**: `onDragOver` calls
/// `setBoard(moveCard(...))` (ProjectBoardPage.tsx:230-234). dnd-kit computes
/// `overId` during render out of the *measured* rects (core.esm.js:2985-2992)
/// and fires `onDragOver` from an effect keyed on `[overId]`
/// (core.esm.js:3286). Mutating the board re-parents card droppables, and a
/// changed droppable set makes dnd-kit re-measure every rect
/// (`useDroppableMeasuring`, the `containersRef.current !== containers`
/// branch). So the preview changes the geometry that decides the preview,
/// under a cursor that never moved — a commit -> setState -> commit loop.
/// React counts those as nested updates and throws at 50: "Maximum update
/// depth exceeded", minified error #185.
///
/// This file models only the browser (layout) and dnd-kit (measure, fire on
/// change). Every decision is the project's own code: `filterBoard`,
/// `boardCollisionDetection`, `resolveDrop`, `moveCard`.

// ---------------------------------------------------------------------------
// Layout engine — the board's real geometry (ProjectBoardPage.tsx:470-716)
// ---------------------------------------------------------------------------

/// `p-4` on the board scroller.
const BOARD_PAD = 16;
/// `min-w-[210px]` per column, `gap-3` between them, `border-2` around each.
const COLUMN_W = 210;
const COLUMN_GAP = 12;
const BORDER = 2;
const HEADER_H = 37;
/// `p-2` inside the list, `gap-2` between cards.
const LIST_PAD = 8;
const CARD_H = 90;
const CARD_GAP = 8;
const PITCH = CARD_H + CARD_GAP;
/// The list is a fixed viewport (`flex-1` under `max-h-full`, scrolling its
/// own overflow): it does not grow when a card is added to it.
const LIST_H = 620;
const LIST_TOP = BOARD_PAD + BORDER + HEADER_H;

function rect(left: number, top: number, width: number, height: number): ClientRect {
  return { left, top, width, height, right: left + width, bottom: top + height };
}

/// The column's droppable is the whole `<section>` — borders and header
/// included (`ref={setNodeRef}` on the section, ProjectBoardPage.tsx).
function columnBox(index: number): ClientRect {
  const left = BOARD_PAD + index * (COLUMN_W + COLUMN_GAP);
  return rect(left, BOARD_PAD, COLUMN_W, 2 * BORDER + HEADER_H + LIST_H);
}

/// The scroller inside it, which is where the cards actually sit.
function listBox(index: number): ClientRect {
  return rect(
    columnBox(index).left + BORDER,
    LIST_TOP,
    COLUMN_W - 2 * BORDER,
    LIST_H,
  );
}

function cardBox(columnIndex: number, slot: number): ClientRect {
  const box = listBox(columnIndex);
  return rect(
    box.left + LIST_PAD,
    box.top + LIST_PAD + slot * PITCH,
    box.width - 2 * LIST_PAD,
    CARD_H,
  );
}

const CARD_W = cardBox(0, 0).width;

function columnX(index: number): number {
  const box = columnBox(index);
  return box.left + box.width / 2;
}

/// The middle of the seam between two columns — the 12px of `gap-3` that no
/// droppable covers.
function gutterX(leftIndex: number): number {
  return columnBox(leftIndex).right + COLUMN_GAP / 2;
}

/// The vertical middle of the `slot`-th card row.
function slotY(slot: number): number {
  return LIST_TOP + LIST_PAD + slot * PITCH + CARD_H / 2;
}

// ---------------------------------------------------------------------------
// dnd-kit's plumbing
// ---------------------------------------------------------------------------

type CollisionArgs = Parameters<CollisionDetection>[0];

/// Registration order of the droppables — the iteration order of dnd-kit's
/// `DroppableContainersMap`, and therefore the tie-break of every collision
/// sort.
///
/// Effects run child-first, so a column's cards register before the column
/// itself and the columns register left to right. A card that changes column
/// unmounts and remounts, so it re-registers at the **back** of the map.
class Registry {
  private order: string[] = [];

  sync(view: Board): void {
    const live = new Set(this.ids(view));
    this.order = this.order.filter((id) => live.has(id));
    for (const id of this.ids(view)) {
      if (!this.order.includes(id)) this.order.push(id);
    }
  }

  private ids(view: Board): string[] {
    const ids: string[] = [];
    for (const status of COLUMNS) {
      for (const issue of view[status]) ids.push(cardDragId(issue.number));
      ids.push(columnDropId(status));
    }
    return ids;
  }

  sorted(): readonly string[] {
    return this.order;
  }
}

/// Measure the rendered view the way the browser would lay it out. Rects are
/// transform-agnostic in dnd-kit's default measuring config, so the sortable
/// animation offsets play no part: a card's rect is its slot.
function measure(
  view: Board,
  registry: Registry,
): { rects: Map<string, ClientRect>; containers: DroppableContainer[] } {
  registry.sync(view);
  const rects = new Map<string, ClientRect>();
  COLUMNS.forEach((status, index) => {
    rects.set(columnDropId(status), columnBox(index));
    view[status].forEach((issue, slot) => {
      rects.set(cardDragId(issue.number), cardBox(index, slot));
    });
  });
  const containers = registry
    .sorted()
    .map((id) => ({ id, key: id, disabled: false }) as unknown as DroppableContainer);
  return { rects, containers };
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

/// What the board shipped before the fix: the same rules, but with the
/// keyboard sensor's `rectIntersection` reached by a live cursor too, whenever
/// it was over no droppable. Kept so the sweep below cannot pass for the wrong
/// reason — it is the counterfactual, not a policy the board has a switch for.
const legacyTargeting: CollisionDetection = (args) => {
  const scoped = {
    ...args,
    droppableContainers: args.droppableContainers.filter(
      (container) => container.id !== args.active.id,
    ),
  };
  const under = pointerWithin(scoped);
  return under.length > 0 ? boardCollisionDetection(args) : rectIntersection(scoped);
};

type Scenario = {
  name: string;
  board: Board;
  filter: BoardFilter;
  activeNumber: number;
  pointer: { x: number; y: number };
  /// Where in the card it was grabbed. The DragOverlay rides the cursor, so
  /// its rect does **not** move when the board re-flows underneath it.
  grab: { x: number; y: number };
  /// The keyboard sensor moves a card with no cursor at all, which is the one
  /// case that still judges by the dragged rectangle.
  blind?: boolean;
};

type Step = { overId: string | null; columns: string };

type Outcome = {
  converged: boolean;
  steps: Step[];
  /// Index into `steps` of the first repeat — the head of the cycle.
  cycleStart: number | null;
};

/// React throws at 50 nested updates; 60 is past the point of no return.
const CAP = 60;

function columnSignature(view: Board): string {
  return COLUMNS.filter((status) => view[status].length > 0)
    .map((status) => `${status}[${view[status].map((issue) => issue.number).join(',')}]`)
    .join(' ');
}

function simulate(
  scenario: Scenario,
  targeting: CollisionDetection = boardCollisionDetection,
): Outcome {
  const activeId = cardDragId(scenario.activeNumber);
  let board = scenario.board;
  const registry = new Registry();
  // Measured once at drag start, then rigidly attached to the cursor.
  const collisionRect = rect(
    scenario.pointer.x - scenario.grab.x,
    scenario.pointer.y - scenario.grab.y,
    CARD_W,
    CARD_H,
  );

  let lastOverId: string | null | undefined;
  const steps: Step[] = [];
  const seen = new Map<string, number>();

  for (let iteration = 0; iteration < CAP; iteration += 1) {
    const view = filterBoard(board, scenario.filter, []);
    const { rects, containers } = measure(view, registry);
    const args: CollisionArgs = {
      active: { id: activeId } as Active,
      collisionRect,
      droppableRects: rects,
      droppableContainers: containers,
      pointerCoordinates: scenario.blind === true ? null : scenario.pointer,
    };
    const first = targeting(args).at(0);
    const overId = first === undefined ? null : String(first.id);

    // dnd-kit fires `onDragOver` only when `overId` changes — its effect's
    // dep array is `[overId]` (core.esm.js:3286). An unchanged target is the
    // drag settling.
    if (overId === lastOverId) return { converged: true, steps, cycleStart: null };

    // Registration order is part of the state: a re-registered card moves to
    // the back of the map, which can flip a tie.
    const key = `${overId} | ${columnSignature(view)} | ${registry.sorted().join('>')}`;
    const repeat = seen.get(key);
    if (repeat !== undefined) return { converged: false, steps, cycleStart: repeat };
    seen.set(key, steps.length);

    lastOverId = overId;
    const drop = resolveDrop(view, activeId, overId);
    if (drop !== null) board = moveCard(board, drop);
    steps.push({ overId, columns: columnSignature(filterBoard(board, scenario.filter, [])) });
  }
  return { converged: false, steps, cycleStart: null };
}

// ---------------------------------------------------------------------------
// Boards
// ---------------------------------------------------------------------------

function issue(number: number, status: IssueStatus, overrides: Partial<Issue> = {}): Issue {
  return {
    number,
    project_id: 'p',
    title: `issue ${number}`,
    description: '',
    status,
    priority: 'none',
    position: number,
    stage: 0,
    created_at_ms: 0,
    updated_at_ms: 0,
    unread: 0,
    last_run_failed: false,
    ...overrides,
  };
}

/// `counts[i]` cards in column `i`, numbered `10 * i + 1 …`.
function boardOf(counts: readonly number[], cancelled: readonly number[] = []): Board {
  const issues: Issue[] = [];
  COLUMNS.forEach((status, index) => {
    for (let slot = 0; slot < (counts[index] ?? 0); slot += 1) {
      const number = 10 * index + slot + 1;
      issues.push(issue(number, status, cancelled.includes(number) ? { cancelled_at_ms: 1 } : {}));
    }
  });
  return groupByStatus(issues);
}

// ---------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------

type Finding = { scenario: Scenario; outcome: Outcome };

/// What a sweep found, without keeping every finding alive: a count, where
/// they were, and the smallest cycle among them to print.
type Sweep = { found: number; zones: Record<string, number>; worst: Finding | null };

/// Where a cursor is, in the only terms that matter here: on a column, or on
/// board that belongs to no droppable at all.
type Zone = 'on a column' | 'in a column gutter' | 'above the columns' | 'below the columns';

function zoneOf(pointer: { x: number; y: number }): Zone {
  const column = COLUMNS.map((_, index) => columnBox(index)).find(
    (box) => pointer.x >= box.left && pointer.x <= box.right,
  );
  if (column === undefined) return 'in a column gutter';
  if (pointer.y < column.top) return 'above the columns';
  if (pointer.y > column.bottom) return 'below the columns';
  return 'on a column';
}

const LAYOUTS: Array<{ counts: number[]; cancelled?: number[]; filter?: BoardFilter }> = [
  { counts: [3, 3, 3, 0, 0] },
  { counts: [4, 2, 3, 0, 0] },
  { counts: [6, 6, 6, 0, 0] },
  { counts: [2, 5, 4, 0, 0] },
  { counts: [4, 0, 4, 0, 0] },
  { counts: [1, 1, 1, 0, 0] },
  // A filtered view: two cancelled cards in Todo that are not rendered, so the
  // board `onDragOver` mutates and the layout the cursor sees disagree.
  {
    counts: [4, 4, 4, 0, 0],
    cancelled: [12, 13],
    filter: { ...EMPTY_FILTER, showCancelled: false },
  },
];

function pointerGrid(): Array<{ x: number; y: number; where: string }> {
  const points: Array<{ x: number; y: number; where: string }> = [];
  const ys: number[] = [];
  // Every 7px: coarse enough to keep the sweep inside a unit test's budget,
  // fine enough that no band of the board goes unsampled — the narrowest one
  // that answers nothing is the 12px between two columns, and that is an x.
  for (let y = BOARD_PAD - 30; y <= LIST_TOP + LIST_H + 30; y += 7) ys.push(y);
  for (let column = 0; column < 3; column += 1) {
    for (const y of ys) {
      points.push({ x: columnX(column), y, where: `column ${column} middle` });
      points.push({ x: columnBox(column).left + 3, y, where: `column ${column} left edge` });
      points.push({ x: columnBox(column).right - 3, y, where: `column ${column} right edge` });
    }
    if (column < 2) {
      for (const y of ys) {
        points.push({ x: gutterX(column), y, where: `gutter ${column}/${column + 1}` });
      }
    }
  }
  return points;
}

/// Where the card was grabbed, which is where the overlay sits relative to the
/// cursor: middle, and by the top-left corner (a card grabbed by its header).
const GRABS = [
  { grab: { x: CARD_W / 2, y: CARD_H / 2 }, name: 'centre' },
  { grab: { x: 20, y: 12 }, name: 'top-left' },
];

/// The steps a drag repeats forever, and nothing at all when it settled.
function cycleOf(outcome: Outcome): Step[] {
  if (outcome.converged) return [];
  return outcome.cycleStart === null ? outcome.steps : outcome.steps.slice(outcome.cycleStart);
}

/// The shorter cycle of two, and the one on the smaller board when they tie.
function worse(best: Finding | null, candidate: Finding): Finding {
  if (best === null) return candidate;
  const size = (finding: Finding) => cycleOf(finding.outcome).length;
  const cards = (finding: Finding) =>
    COLUMNS.reduce((total, status) => total + finding.scenario.board[status].length, 0);
  if (size(candidate) < size(best)) return candidate;
  if (size(candidate) > size(best)) return best;
  return cards(candidate) < cards(best) ? candidate : best;
}

/// A summary rather than every finding: a sweep that fails finds tens of
/// thousands of them, and holding each one's trace is how the run dies of
/// memory instead of telling you what it found.
function sweep(
  targeting: CollisionDetection = boardCollisionDetection,
  options: { blind?: boolean } = {},
): Sweep {
  const swept: Sweep = { found: 0, zones: {}, worst: null };
  const grid = pointerGrid();
  for (const layout of LAYOUTS) {
    const board = boardOf(layout.counts, layout.cancelled ?? []);
    const filter = layout.filter ?? EMPTY_FILTER;
    const view = filterBoard(board, filter, []);
    const actives = new Set<number>();
    for (const status of COLUMNS) {
      const column = view[status];
      if (column.length === 0) continue;
      actives.add(column[0].number);
      actives.add(column[column.length - 1].number);
    }
    for (const activeNumber of actives) {
      for (const point of grid) {
        for (const { grab, name } of GRABS) {
          const scenario: Scenario = {
            name: `${layout.counts.join('/')} · drag #${activeNumber} · ${point.where} y=${point.y} · grabbed ${name}`,
            board,
            filter,
            activeNumber,
            pointer: { x: point.x, y: point.y },
            grab,
            blind: options.blind,
          };
          const outcome = simulate(scenario, targeting);
          if (outcome.converged) continue;
          const zone = zoneOf(scenario.pointer);
          swept.found += 1;
          swept.zones[zone] = (swept.zones[zone] ?? 0) + 1;
          swept.worst = worse(swept.worst, { scenario, outcome });
        }
      }
    }
  }
  return swept;
}

/// What a sweep has to say for itself, which on a healthy board is nothing.
function complaint(swept: Sweep): string[] {
  if (swept.worst === null) return [];
  const { scenario, outcome } = swept.worst;
  return [
    `${swept.found} non-converging (board, cursor) pairs: ${JSON.stringify(swept.zones)}`,
    scenario.name,
    `  zone: ${zoneOf(scenario.pointer)} — cursor (${scenario.pointer.x}, ${scenario.pointer.y})`,
    `  start: ${columnSignature(filterBoard(scenario.board, scenario.filter, []))}`,
    ...cycleOf(outcome).map(
      (step, index) => `  ${index + 1}. over=${step.overId} -> ${step.columns}`,
    ),
  ];
}

// ---------------------------------------------------------------------------

describe('drag preview convergence', () => {
  /// The gesture that crashed the board: the top card of a full column picked
  /// up, and the cursor parked in the 12px seam on its way to the next column
  /// — which is where the cursor *is* for a frame on any drag from one column
  /// to the next.
  ///
  /// The dragged card was the only thing at the cursor's height in its own
  /// column, so taking it out promoted the card behind it into the dragged
  /// rectangle's band, and the target flipped back. Nothing about the board is
  /// unusual; it is the smallest realistic drag there is.
  it('a cursor parked in the seam between two columns settles', () => {
    const scenario: Scenario = {
      name: 'seam',
      board: boardOf([3, 3, 0, 0, 0]),
      filter: EMPTY_FILTER,
      activeNumber: 1,
      pointer: { x: gutterX(0), y: slotY(0) },
      grab: { x: CARD_W / 2, y: CARD_H / 2 },
    };
    expect(cycleOf(simulate(scenario)).map((step) => step.overId)).toEqual([]);
    // …and the fallback is what did it, so this cannot pass for another reason.
    expect(cycleOf(simulate(scenario, legacyTargeting)).length).toBeGreaterThan(0);
  });

  /// The other half of that dead zone was the band across the column headers,
  /// the full width of the board — and the crash there was the worse one,
  /// because the top of a column is where a card headed for the front of the
  /// queue is aimed. The column's droppable reaches over its own header now,
  /// so that aim lands on the column, and the card goes in at the top of it.
  it('a cursor over a column header aims at that column', () => {
    const scenario: Scenario = {
      name: 'header',
      board: boardOf([2, 2, 0, 0, 0]),
      filter: EMPTY_FILTER,
      activeNumber: 1,
      pointer: { x: columnX(1), y: BOARD_PAD + 8 },
      grab: { x: 20, y: 12 },
    };
    const outcome = simulate(scenario);
    expect(cycleOf(outcome)).toEqual([]);
    expect(outcome.steps.at(-1)?.columns).toBe('backlog[2] todo[1,11,12]');
  });

  /// The property the board needs, over every layout, every grab and every
  /// pixel of the grid: a cursor that has stopped moving stops the preview.
  /// The preview mutates the board, dnd-kit re-measures on the mutation, and
  /// the new measurement decides the next preview — so a target that is not a
  /// fixed point of that exchange is a live lock, and React throws #185 at 50
  /// commits.
  it('no board and no stationary cursor drives the preview forever', () => {
    expect(complaint(sweep())).toEqual([]);
  });

  /// The sweep above is worth something only if it can fail, so here is the
  /// same grid against the rule that shipped — the one whose fallback judged
  /// by the dragged rectangle wherever the cursor was over nothing.
  it('the sweep finds the crash it was written for', () => {
    const swept = sweep(legacyTargeting);
    expect({ cycles: swept.found > 0, seam: 'in a column gutter' in swept.zones }).toEqual({
      cycles: true,
      seam: true,
    });
  });

  /// The keyboard sensor moves a card with no cursor at all, so the dragged
  /// rectangle is all it has — the one place that fallback is still reached.
  /// It settles because the rectangle moves only when a key is pressed, and it
  /// is measured against the board the last keypress produced.
  it('a keyboard drag settles too', () => {
    expect(complaint(sweep(boardCollisionDetection, { blind: true }))).toEqual([]);
  });
});
