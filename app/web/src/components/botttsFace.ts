import { createAvatar } from '@dicebear/core';
import * as bottts from '@dicebear/bottts';

/// The face an agent wears until somebody gives it one.
///
/// Drawn here rather than fetched from `api.dicebear.com`: this dashboard is
/// served off a box that need not have a route to the internet, and every
/// request would hand an agent's id to a third party in exchange for a
/// picture we can draw ourselves.
///
/// The seed is the agent **profile id**, not the handle and not the name.
/// It is the only identity the board's roster and the `/agents` page have in
/// common, and the only one that never changes — so a teammate keeps the
/// same face on both pages, and keeps it after a rename.
///
/// The artwork is Pablo Stanley's Bottts, free for personal and commercial
/// use. The credit rides in the `<metadata>` block DiceBear writes into every
/// SVG, which is why nothing here trims it to save the ~640 bytes.

/// Backgrounds the robot is placed on. DiceBear's own palette is a saturated
/// rainbow; these are the board's warm tints, so a generated face lands
/// inside the design system rather than beside it.
const BACKGROUNDS = ['aecbdd', 'd9bfd4', 'cdd4ab', 'e5c9a0'];

/// Same seed, same face — so drawing one is a lookup after the first time.
/// An avatar is re-rendered on every board poll, and each draw is ~4 kB of
/// SVG to serialise.
const drawn = new Map<string, string>();

export function botttsFace(seed: string): string {
  const already = drawn.get(seed);
  if (already !== undefined) return already;
  const uri = createAvatar(bottts, { seed, backgroundColor: BACKGROUNDS }).toDataUri();
  drawn.set(seed, uri);
  return uri;
}
