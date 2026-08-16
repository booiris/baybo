// Excerpt + highlight for a search result card.
//
// Kept out of the component so the rules are testable without a DOM: which
// terms the server actually matched, and where to cut, is the whole feature.
// See `docs/search.md`.
//
// Mirrored in Swift for the iOS search screen (`SearchSnippet.swift`), against
// the shared vectors in `searchSnippetVectors.json`. Everything below works in
// GRAPHEME CLUSTER space for that reason — see `graphemes`.

/** Prose kept BEFORE the match.
 *
 *  Deliberately short, and the reason is the card rather than the prose: the
 *  excerpt renders `line-clamp-2` in a 260px sidebar, which is ~34 full-width
 *  CJK (or ~58 ASCII) characters. Anything longer than that in front of the
 *  match pushes the highlight past the clamp, and the card shows a match the
 *  reader cannot see — measured on live data at a 60-character lead, 10 of 13
 *  results for one query rendered without the searched term on screen.
 *
 *  Counted in grapheme clusters, not UTF-16 code units: it is a reading-length
 *  budget, and these are the numbers the Swift port has to agree with. */
const LEAD_PAD = 12;

/** And AFTER it — the rest of the same budget. Overflowing the clamp on this
 *  side costs nothing: it is the side the reader never reaches. */
const TRAIL_PAD = 108;

/** Head shown when no term can be located — see `snippet`. */
const HEAD_LEN = LEAD_PAD + TRAIL_PAD;

/** A run of the message, flagged if it is one of the query's terms. */
export interface Segment {
  text: string;
  match: boolean;
}

const SEGMENTER = new Intl.Segmenter(undefined, { granularity: 'grapheme' });

/**
 * Split text into grapheme clusters — what a reader calls "a character".
 *
 * Every index in this file is an index into one of these arrays, and that is
 * deliberate. Slicing a JS string cuts UTF-16 code units, so a window edge
 * landing mid-surrogate emits a lone surrogate (which `JSON.stringify` will
 * happily produce and a strict decoder will reject), and one landing inside a
 * ZWJ sequence shatters one emoji into several. Swift's `String` is already
 * grapheme-indexed, so working here is also the only way the two ports can
 * agree byte for byte instead of only on the BMP.
 */
function graphemes(text: string): string[] {
  return Array.from(SEGMENTER.segment(text), (s) => s.segment);
}

/**
 * Split a query the way the server does.
 *
 * The user's own whitespace is the AND boundary — the server emits one literal
 * phrase per chunk and ANDs them — and a chunk with nothing alphanumeric
 * contributes no tokens and is dropped rather than searched for. Mirroring that
 * split is the point: a query is a set of independent terms, not a substring.
 */
export function queryChunks(query: string): string[] {
  return query.split(/\s+/).filter((chunk) => /[\p{L}\p{N}]/u.test(chunk));
}

/**
 * Case-fold cluster by cluster, never the string as a whole.
 *
 * Folding the whole string can change its LENGTH (`İ` lowercases to two code
 * points), which silently slides every subsequent offset — so matching on a
 * folded string and slicing the original is only correct until someone types
 * Turkish. Folding per cluster keeps the folded array index-aligned with the
 * original one by construction, whatever any individual cluster does.
 */
function folded(clusters: string[]): string[] {
  return clusters.map((c) => c.toLowerCase());
}

/** Grapheme indices where `needle` occurs in `haystack` (both already folded). */
function occurrences(haystack: string[], needle: string[]): number[] {
  const out: number[] = [];
  if (needle.length === 0) return out;
  for (let at = 0; at + needle.length <= haystack.length; at += 1) {
    let hit = true;
    for (let k = 0; k < needle.length; k += 1) {
      if (haystack[at + k] !== needle[k]) {
        hit = false;
        break;
      }
    }
    if (hit) {
      out.push(at);
      at += needle.length - 1;
    }
  }
  return out;
}

/**
 * Cut a window of `text` around what the query matched, flagging the terms.
 *
 * Anchors on the EARLIEST term rather than searching for the whole input:
 * `foo bar` matches a message reading `bar … foo`, or one where the two sit 500
 * characters apart, because the server ANDs them independently. Looking for the
 * literal `"foo bar"` finds nothing there and would fall back to the head — a
 * genuine result rendered with an excerpt containing none of what was searched
 * for.
 *
 * Matching is a plain case-insensitive substring, deliberately, and it agrees
 * with the index for the same reason: a phrase of character-unigrams IS the
 * substring the user typed. Two server behaviours it does not reproduce, both
 * degrading to a head excerpt rather than a wrong one:
 *   - prefix widening (`session` reaches `sessions`) — still found, since the
 *     query is a prefix of the word;
 *   - `unicode61`'s diacritic folding (`resume` reaches `résumé`) — not found.
 */
export function snippet(text: string, query: string): Segment[] {
  const g = graphemes(text);
  const gl = folded(g);
  const chunks = queryChunks(query);

  const hits: Array<{ at: number; end: number }> = [];
  for (const chunk of chunks) {
    const needle = folded(graphemes(chunk));
    for (const at of occurrences(gl, needle)) {
      hits.push({ at, end: at + needle.length });
    }
  }
  const cut = (from: number, to: number) => g.slice(from, to).join('');

  if (hits.length === 0) {
    const head = cut(0, HEAD_LEN);
    return [{ text: head + (g.length > HEAD_LEN ? '…' : ''), match: false }];
  }

  hits.sort((a, b) => a.at - b.at);
  const anchor = hits[0];
  const from = Math.max(0, anchor.at - LEAD_PAD);
  const to = Math.min(g.length, anchor.end + TRAIL_PAD);

  const segments: Segment[] = [];
  const push = (t: string, match: boolean) => {
    if (t.length > 0) segments.push({ text: t, match });
  };

  let cursor = from;
  for (const hit of hits) {
    // Highlight every term that lands in the window, not just the anchor — a
    // two-word query usually has its other term right there. Overlapping hits
    // (a term inside another) collapse into the first.
    if (hit.at < cursor || hit.end > to) continue;
    push(cut(cursor, hit.at), false);
    push(cut(hit.at, hit.end), true);
    cursor = hit.end;
  }
  push(cut(cursor, to), false);

  if (from > 0) segments.unshift({ text: '…', match: false });
  if (to < g.length) segments.push({ text: '…', match: false });
  return segments;
}
