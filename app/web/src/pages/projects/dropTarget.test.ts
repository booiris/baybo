import { describe, expect, it } from 'vitest';
import {
  closestCorners,
  type Active,
  type ClientRect,
  type CollisionDetection,
  type DroppableContainer,
} from '@dnd-kit/core';

import { boardCollisionDetection } from './dropTarget';
import { cardDragId, columnDropId } from './boardModel';

type CollisionArgs = Parameters<CollisionDetection>[0];

function rect(left: number, top: number, width: number, height: number): ClientRect {
  return { left, top, width, height, right: left + width, bottom: top + height };
}

/// Two 250px columns side by side, 10px apart, cards inset 10px and 90px tall
/// with an 8px gap. The source column is a full stack; the target column holds
/// one card at the top and is empty below it — which is the layout the old
/// strategy could not cross.
const SOURCE = columnDropId('backlog');
const TARGET = columnDropId('todo');
const LAYOUT: Array<[string, ClientRect]> = [
  [SOURCE, rect(0, 100, 250, 600)],
  [cardDragId(1), rect(10, 110, 230, 90)],
  [cardDragId(2), rect(10, 208, 230, 90)],
  [cardDragId(3), rect(10, 404, 230, 90)],
  [TARGET, rect(260, 100, 250, 600)],
  [cardDragId(9), rect(270, 110, 230, 90)],
];

function args(dragged: string, card: ClientRect, cursor: { x: number; y: number } | null) {
  return {
    active: { id: dragged } as Active,
    collisionRect: card,
    droppableRects: new Map(LAYOUT),
    droppableContainers: LAYOUT.map(
      ([id]) => ({ id, key: id, disabled: false }) as unknown as DroppableContainer,
    ),
    pointerCoordinates: cursor,
  } satisfies CollisionArgs;
}

function target(dragged: string, card: ClientRect, cursor: { x: number; y: number } | null) {
  const first = boardCollisionDetection(args(dragged, card, cursor)).at(0);
  return first === undefined ? null : String(first.id);
}

/// What shipped before, so the two tests that claim a fix cannot pass for the
/// wrong reason.
function before(dragged: string, card: ClientRect, cursor: { x: number; y: number } | null) {
  const first = closestCorners(args(dragged, card, cursor)).at(0);
  return first === undefined ? null : String(first.id);
}

describe('boardCollisionDetection', () => {
  it('crosses into a column that is empty at the height being crossed at', () => {
    // Card 3 is the bottom of a full column; the target column has nothing at
    // that height. The card is two thirds of the way across.
    const dragged = rect(170, 404, 230, 90);
    const cursor = { x: 285, y: 449 };

    // The old strategy answered with the dragged card's own slot, which
    // `resolveDrop` refuses — so the board did not respond at all.
    expect(before(cardDragId(3), dragged, cursor)).toBe(cardDragId(3));
    expect(target(cardDragId(3), dragged, cursor)).toBe(TARGET);
  });

  it('never answers with the card being dragged', () => {
    // Directly over its own slot, having barely moved: the honest answer is
    // the column it is still in, not itself.
    const home = rect(10, 404, 230, 90);
    expect(before(cardDragId(3), home, { x: 125, y: 449 })).toBe(cardDragId(3));
    expect(target(cardDragId(3), home, { x: 125, y: 449 })).toBe(SOURCE);
  });

  it('prefers the card under the cursor over the column holding it', () => {
    // Both are hit. Only the card says *where* in the column it goes.
    expect(target(cardDragId(3), rect(270, 110, 230, 90), { x: 300, y: 150 })).toBe(cardDragId(9));
  });

  it('picks the nearer card when the cursor is in a seam between cards', () => {
    // y=200..208 is the gap between cards 1 and 2 and belongs to neither.
    // Answering with the column there would mean "append", flicking the
    // preview to the bottom on every card boundary swept past — so the answer
    // has to stay a card, and has to change sides continuously across the gap.
    const dragged = rect(10, 160, 230, 90);
    expect(target(cardDragId(3), dragged, { x: 125, y: 201 })).toBe(cardDragId(1));
    expect(target(cardDragId(3), dragged, { x: 125, y: 207 })).toBe(cardDragId(2));
    // The column's own 10px of side padding is the same kind of seam: no card
    // reaches it, and it runs the full height of the column.
    expect(target(cardDragId(3), dragged, { x: 4, y: 150 })).toBe(cardDragId(1));
  });

  it('asks for the end of the column below the last card', () => {
    // Past every card is the one place the column itself is the right answer
    // — the operator is asking for the end, not for the card above it. The
    // seam rule above must not reach down here and grab card 9.
    expect(target(cardDragId(3), rect(270, 455, 230, 90), { x: 300, y: 500 })).toBe(TARGET);
  });

  it('falls back to rectangles when there is no cursor at all', () => {
    // The keyboard sensor moves the card without a pointer, so `pointerWithin`
    // finds nothing and the dragged rectangle has to decide.
    expect(target(cardDragId(3), rect(270, 110, 230, 90), null)).toBe(cardDragId(9));
  });
});
