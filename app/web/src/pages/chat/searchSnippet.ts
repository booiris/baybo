// Excerpt + highlight for a search result card.
//
// Kept out of the component so the rules are testable without a DOM: which
// terms the server actually matched, and where to cut, is the whole feature.
// See `docs/search.md`.

/** Characters of prose either side of the match. Enough to recognise the
 *  moment, short enough that a result stays one glanceable card. */
const SNIPPET_PAD = 60;

/** Head shown when no term can be located — see `snippet`. */
const HEAD_LEN = SNIPPET_PAD * 2;

/** A run of the message, flagged if it is one of the query's terms. */
export interface Segment {
  text: string;
  match: boolean;
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

/** Every occurrence of `needle` in `haystack` (both already lowercased). */
function occurrences(haystack: string, needle: string): number[] {
  const out: number[] = [];
  for (let at = haystack.indexOf(needle); at >= 0; at = haystack.indexOf(needle, at + needle.length)) {
    out.push(at);
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
  const lower = text.toLowerCase();
  const chunks = queryChunks(query);

  const hits: Array<{ at: number; end: number }> = [];
  for (const chunk of chunks) {
    for (const at of occurrences(lower, chunk.toLowerCase())) {
      hits.push({ at, end: at + chunk.length });
    }
  }
  if (hits.length === 0) {
    const head = text.slice(0, HEAD_LEN);
    return [{ text: head + (text.length > head.length ? '…' : ''), match: false }];
  }

  hits.sort((a, b) => a.at - b.at);
  const anchor = hits[0];
  const from = Math.max(0, anchor.at - SNIPPET_PAD);
  const to = Math.min(text.length, anchor.end + SNIPPET_PAD);

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
    push(text.slice(cursor, hit.at), false);
    push(text.slice(hit.at, hit.end), true);
    cursor = hit.end;
  }
  push(text.slice(cursor, to), false);

  if (from > 0) segments.unshift({ text: '…', match: false });
  if (to < text.length) segments.push({ text: '…', match: false });
  return segments;
}
