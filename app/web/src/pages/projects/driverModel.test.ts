import { describe, expect, it } from 'vitest';

import { agentsMayMergeHint } from './driverModel';

describe('agentsMayMergeHint', () => {
  // "Agents may merge" alone reads as a permission with no consequence, and
  // the consequence is the whole setting: with it on, a reviewed card's
  // branch reaches the repository without anyone running git.
  it('says what each setting does, not just that it is on', () => {
    expect(agentsMayMergeHint(true)).toContain('reviewed');
    expect(agentsMayMergeHint(true)).toContain('merges');
    expect(agentsMayMergeHint(false)).toContain('hands over its branch');
  });

  // It is not a lock: a run carries a shell and a writable checkout. The off
  // state promises only what it can keep — that the tool refuses.
  it('promises a refusal rather than a prohibition when it is off', () => {
    expect(agentsMayMergeHint(false)).toContain('IssueMerge refuses');
  });
});
