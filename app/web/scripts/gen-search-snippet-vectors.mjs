// Regenerate `src/pages/chat/searchSnippetVectors.json` — the one contract the
// Swift port of `searchSnippet.ts` is held to.
//
//   pnpm --filter baybo-web gen:snippet-vectors
//
// The JS implementation is the REFERENCE: its own rules are pinned by the
// hand-written cases in `searchSnippet.test.ts`, and these vectors pin the exact
// bytes both ports must produce for those rules. Regenerating is therefore only
// correct once the hand-written suite is green — otherwise this bakes a bug into
// the contract and the Swift side dutifully reproduces it.
//
// Expect `SearchSnippetVectorTests` (app/ios) to go red after a regen until the
// Swift side is ported to match. That red is the gate working; app/ios has no
// CI (every `ios-*` job is `if: false`), so run it by hand.

import { writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

import { snippet } from '../src/pages/chat/searchSnippet.ts';

/** `[name, text, query]`. Keep names stable — the Swift suite reports by name. */
const cases = [
  ['single CJK term, whole text fits', '先说说数据库的事情', '数据库'],
  ['two terms both in window', '聊聊数据库的迁移方案', '数据库 迁移'],
  [
    'anchors on the earliest term, not the first typed',
    `${'x'.repeat(80)}迁移${'y'.repeat(200)}数据库`,
    '数据库 迁移',
  ],
  ['case-insensitive, original casing preserved', 'The Session started', 'session'],
  ['repeats of one term', 'foo and foo again', 'foo'],
  ['prefix widening still locates the typed prefix', 'list the sessions please', 'session'],
  ['no locatable term falls back to the head', 'résumé of the work', 'resume'],
  ['head fallback elides when longer than the head budget', 'a'.repeat(200), 'zzz'],
  ['interior window elides both ends', `${'a'.repeat(100)}检索${'b'.repeat(200)}`, '检索'],
  ['punctuation-only query yields the head', 'some text', '---'],
  ['empty query yields the head', 'some text', ''],
  ['empty text', '', 'foo'],
  // The grapheme cases — every one of these is a window edge a UTF-16 slice
  // gets wrong. A family emoji is ONE cluster of 7 code points; a flag is a
  // surrogate pair; NFD `é` is `e` + U+0301.
  ['ZWJ emoji sits exactly on the leading window edge', `${'👨‍👩‍👧‍👦'.repeat(70)}检索`, '检索'],
  ['ZWJ emoji sits exactly on the trailing window edge', `检索${'👨‍👩‍👧‍👦'.repeat(120)}`, '检索'],
  ['surrogate-pair flags around a match', `${'🇨🇳'.repeat(70)}检索${'🇯🇵'.repeat(120)}`, '检索'],
  ['emoji inside the matched term', '说说 🎉派对 的事', '🎉派对'],
  // Escaped, not a literal `é`: an NFC-normalizing editor pass silently turns
  // the decomposed literal into the precomposed code point and the case stops
  // testing anything. It happened once already.
  ['decomposed combining mark (e+U+0301) stays with its base', `${'e\u0301'.repeat(70)}检索`, '检索'],
];

const vectors = cases.map(([name, text, query]) => ({
  name,
  text,
  query,
  segments: snippet(text, query),
}));

const out = join(
  dirname(fileURLToPath(import.meta.url)),
  '../src/pages/chat/searchSnippetVectors.json',
);
writeFileSync(out, `${JSON.stringify(vectors, null, 2)}\n`);
console.log(`wrote ${vectors.length} vectors to ${out}`);
