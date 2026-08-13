// Landing a search hit in the thread: which row to scroll to, and painting the
// query's terms once we are there. See `docs/web-chat.md`.
//
// Kept out of the component so the resolution rule is testable without a chat
// page: the ordinal a hit carries is NOT always a row of its own, and getting
// that wrong scrolls to the wrong turn.

import type { components } from '../../api/schema';
import { queryChunks } from './searchSnippet';

/** Highlight registry entry, styled by `::highlight(chat-search-hit)`. */
const HIGHLIGHT_NAME = 'chat-search-hit';

/**
 * The ordinal opening this hit should land on.
 *
 * A hit that compaction replaced is not on screen in the conversation — the
 * chat view renders only live rows — so `superseded_by`, the row that took its
 * place, is the only ordinal there is to jump to. The excerpt's `compacted`
 * badge is what tells the reader the landing row is not the matched text.
 */
export function jumpOrdinal(hit: components['schemas']['ChatSearchHit']): number {
  return hit.superseded_by ?? hit.ordinal;
}

/**
 * The rendered row that holds `target`.
 *
 * Not `getElementById('…m<target>')`: a searchable message is not always a row.
 * The model's mid-turn prose is persisted at its own ordinal but rendered
 * *inside* the turn's collapsed work card (`w<anchor>`, keyed by the turn's
 * first intermediate ordinal), so a hit on it has no row of its own — the card
 * that swallowed it is the honest landing place. Hence "the last row at or
 * before `target`" rather than an exact lookup.
 *
 * Falls back to the oldest rendered row: after the walk, a target still below
 * everything on screen means the row is not rendered at all, and the top of
 * what we have is closer than leaving the reader wherever they were.
 */
export function anchorRowFor(scroller: HTMLElement, target: number): HTMLElement | null {
  // Document order is ascending ordinal, so the last one at or before the
  // target is the row that holds it.
  const rows = scroller.querySelectorAll<HTMLElement>('[data-ordinal]');
  let best: HTMLElement | null = null;
  for (const row of rows) {
    const ordinal = Number(row.dataset.ordinal);
    if (!Number.isFinite(ordinal)) continue;
    if (ordinal > target) break;
    best = row;
  }
  if (best !== null) return best;
  return rows.length > 0 ? rows[0] : null;
}

function registry(): HighlightRegistry | null {
  return typeof CSS !== 'undefined' && 'highlights' in CSS ? CSS.highlights : null;
}

export function clearSearchHighlight(): void {
  registry()?.delete(HIGHLIGHT_NAME);
}

/**
 * Paint the query's terms inside `root`.
 *
 * The CSS Custom Highlight API rather than wrapping matches in `<mark>`: a
 * bubble's body is React-rendered markdown, and hand-mutating that subtree is
 * undone the moment React re-renders it (a streaming delta, a thumbnail
 * resolving, a re-fold of the work card). A highlight is a set of `Range`s held
 * *outside* the DOM, so nothing in the tree is touched and there is nothing to
 * clobber. Where the API is missing the jump still scrolls and flashes; only
 * the term highlight is lost.
 *
 * Terms are matched per text node, so a term markdown split across elements
 * (`宏**观**`) goes unpainted — the flash still says which row it is.
 */
export function paintSearchHighlight(root: HTMLElement, query: string): void {
  const highlights = registry();
  if (!highlights) return;
  highlights.delete(HIGHLIGHT_NAME);
  // Same split the server ANDs on and the excerpt highlights by, so what lights
  // up here is what matched.
  const chunks = queryChunks(query).map((chunk) => chunk.toLowerCase());
  if (chunks.length === 0) return;

  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  const ranges: Range[] = [];
  for (let node = walker.nextNode(); node !== null; node = walker.nextNode()) {
    const text = (node.textContent ?? '').toLowerCase();
    if (text.length === 0) continue;
    for (const chunk of chunks) {
      for (let at = text.indexOf(chunk); at >= 0; at = text.indexOf(chunk, at + chunk.length)) {
        const range = document.createRange();
        range.setStart(node, at);
        range.setEnd(node, at + chunk.length);
        ranges.push(range);
      }
    }
  }
  if (ranges.length > 0) highlights.set(HIGHLIGHT_NAME, new Highlight(...ranges));
}
