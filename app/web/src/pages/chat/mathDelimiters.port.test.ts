import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';

// The math normalizer is a byte copy across two pnpm projects that cannot share
// a package (`app/ios/web` is its own workspace root), and the iOS project's
// suite runs in no CI job — so this is the only thing that notices when one copy
// is edited and the other is not. Each file opens with a `//` header naming its
// counterpart; everything past that header must match. When a divergence is
// intentional, port it to both and the check goes quiet.
const PAIRS = [
  ['src/pages/chat/mathDelimiters.ts', '../ios/web/src/mathDelimiters.ts'],
  ['src/pages/chat/mathDelimiters.test.ts', '../ios/web/src/mathDelimiters.test.ts'],
] as const;

function bodyAfterHeader(source: string): string {
  const lines = source.split('\n');
  let i = 0;
  while (i < lines.length && (lines[i].startsWith('//') || lines[i] === '')) i++;
  return lines.slice(i).join('\n');
}

describe('math normalizer port fidelity', () => {
  it.each(PAIRS)('keeps %s identical to its iOS original', (webPath, iosPath) => {
    const web = readFileSync(resolve(process.cwd(), webPath), 'utf8');
    const ios = readFileSync(resolve(process.cwd(), iosPath), 'utf8');
    expect(bodyAfterHeader(web)).toBe(bodyAfterHeader(ios));
  });
});
