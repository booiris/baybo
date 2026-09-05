import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';

// These modules are byte copies across two pnpm projects that cannot share a
// package (`app/mobile/web` is its own workspace root), and only part of the iOS
// project's suite runs in CI — so this is what notices when one copy is edited
// and the other is not. Each file opens with a `//` header naming its
// counterpart; everything past that header must match. When a divergence is
// intentional, port it to both and the check goes quiet.
//
// The sync cursor earns its place here for a harder reason than the math
// normalizer's "both clients must render a message identically": web and device
// are two clients running the ONE sync loop over the one transcript pool, and
// docs/sync-protocol.md names this rule as the guard against permanent data
// loss. A rebase-dirty freeze that holds on only one of them loses rows.
const PAIRS = [
  ['src/pages/chat/mathDelimiters.ts', '../mobile/web/src/mathDelimiters.ts'],
  ['src/pages/chat/mathDelimiters.test.ts', '../mobile/web/src/mathDelimiters.test.ts'],
  ['src/pages/chat/syncCursor.ts', '../mobile/web/src/transcript/cursor.ts'],
] as const;

function bodyAfterHeader(source: string): string {
  const lines = source.split('\n');
  let i = 0;
  while (i < lines.length && (lines[i].startsWith('//') || lines[i] === '')) i++;
  return lines.slice(i).join('\n');
}

describe('cross-client port fidelity', () => {
  it.each(PAIRS)('keeps %s identical to its iOS original', (webPath, iosPath) => {
    const web = readFileSync(resolve(process.cwd(), webPath), 'utf8');
    const ios = readFileSync(resolve(process.cwd(), iosPath), 'utf8');
    expect(bodyAfterHeader(web)).toBe(bodyAfterHeader(ios));
  });
});
