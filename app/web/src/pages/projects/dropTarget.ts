import {
  pointerWithin,
  rectIntersection,
  type ClientRect,
  type Collision,
  type CollisionDetection,
} from '@dnd-kit/core';

import { parseDragId } from './boardModel';

type CollisionArgs = Parameters<CollisionDetection>[0];

function isCard(id: Collision['id']): boolean {
  return parseDragId(String(id))?.kind === 'card';
}

function hoist(winner: Collision, hits: Collision[]): Collision[] {
  return [winner, ...hits.filter((hit) => hit.id !== winner.id)];
}

/// The card the cursor is nearest **within one column**, or null when the
/// cursor is below every card in it.
///
/// Cards sit in an 8px gap from each other inside 8px of column padding, and
/// none of that belongs to a card. Without this, sweeping down a column drops
/// into one of those seams every ~90px, the only hit is the column, and a
/// column hit means "append" — the preview would flick to the bottom of the
/// column and back on each seam crossed.
///
/// Below the last card is the one place that is the honest answer: there the
/// operator is asking for the end of the column, not for the card above it.
function cardNearestPointer(args: CollisionArgs, column: ClientRect | undefined): Collision | null {
  const { droppableContainers, droppableRects, pointerCoordinates } = args;
  if (pointerCoordinates === null || column === undefined) return null;

  let nearest: Collision | null = null;
  let shortest = Number.POSITIVE_INFINITY;
  let below = true;

  for (const container of droppableContainers) {
    if (!isCard(container.id)) continue;
    const rect = droppableRects.get(container.id);
    if (rect === undefined) continue;
    // Which column a card belongs to, without asking dnd-kit: cards span
    // their column and no two columns overlap.
    if (rect.left >= column.right || rect.right <= column.left) continue;
    if (pointerCoordinates.y <= rect.bottom) below = false;

    const distance = Math.abs(rect.top + rect.height / 2 - pointerCoordinates.y);
    if (distance < shortest) {
      shortest = distance;
      nearest = { id: container.id, data: { droppableContainer: container, value: distance } };
    }
  }

  return below ? null : nearest;
}

/// What the dragged card would land on.
///
/// Two things went wrong with dnd-kit's `closestCorners`, which this replaces.
///
/// **The card collided with itself.** `useSortable` registers a droppable
/// under the same id as the draggable, so the slot the card was lifted out of
/// is still in the running — sitting at exactly the height the dragged rect is
/// at, which makes it win every distance measure until the card has fully
/// cleared its old column. `resolveDrop` refuses a card landing on itself, so
/// this did not cause a wrong drop; it caused **no** drop. Slide the bottom
/// card of a full column sideways into a column that happens to have no card
/// at that height, and the card goes most of the way across with the board not
/// responding at all. It is excluded here, so the next-best target wins.
///
/// **The cursor had no say.** Corner distance is measured against the dragged
/// rectangle, so where the operator is actually pointing never entered into
/// it. The target is now what the cursor is over, which is how every board of
/// this kind behaves — with the trade that a card grabbed by its far edge
/// leads its cursor, so the aim is the cursor and not the card's leading edge.
///
/// A card always beats the column holding it: the column alone only says which
/// column, and ordering needs the card. Hits are hoisted rather than filtered,
/// so the column stays available behind it.
///
/// `rectIntersection` is the keyboard sensor's path — it moves the card with
/// no cursor at all, so `pointerWithin` finds nothing.
export const boardCollisionDetection: CollisionDetection = (args) => {
  const scoped: CollisionArgs = {
    ...args,
    droppableContainers: args.droppableContainers.filter(
      (container) => container.id !== args.active.id,
    ),
  };

  const under = pointerWithin(scoped);
  const hits = under.length > 0 ? under : rectIntersection(scoped);

  const card = hits.find((hit) => isCard(hit.id));
  if (card !== undefined) return hoist(card, hits);

  const column = hits.at(0);
  if (column === undefined) return hits;
  const nearest = cardNearestPointer(scoped, args.droppableRects.get(column.id));
  return nearest === null ? hits : hoist(nearest, hits);
};
