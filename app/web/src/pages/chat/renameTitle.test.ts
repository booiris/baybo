import { describe, expect, it } from 'vitest';

import {
  MAX_SESSION_TITLE_LEN,
  capTitle,
  seedTitleDraft,
  titleToCommit,
} from './renameTitle';

describe('capTitle', () => {
  it('leaves a title within the cap alone', () => {
    expect(capTitle('Fix login redirect')).toBe('Fix login redirect');
  });

  it('counts code points, not UTF-16 units', () => {
    // A byte- or `.length`-based cap would cut this to half the characters.
    const cjk = '标'.repeat(MAX_SESSION_TITLE_LEN);
    expect(capTitle(cjk)).toBe(cjk);
    expect([...capTitle('🙂'.repeat(MAX_SESSION_TITLE_LEN + 10))]).toHaveLength(
      MAX_SESSION_TITLE_LEN,
    );
  });

  it('truncates past the cap', () => {
    expect(capTitle('x'.repeat(MAX_SESSION_TITLE_LEN + 1))).toHaveLength(
      MAX_SESSION_TITLE_LEN,
    );
  });
});

describe('seedTitleDraft', () => {
  it('prefers the title, falls back to the preview, then empty', () => {
    expect(seedTitleDraft({ title: 'A', last_user_text: 'B' })).toBe('A');
    expect(seedTitleDraft({ last_user_text: 'B' })).toBe('B');
    expect(seedTitleDraft({})).toBe('');
  });

  it('truncates an over-long server-minted title so the draft is committable', () => {
    // Cron fire titles are not bounded by the rename cap; an untruncated seed
    // would be refused with a 400 the user never asked for.
    const long = 'Daily brief · '.repeat(20);
    expect([...seedTitleDraft({ title: long })]).toHaveLength(MAX_SESSION_TITLE_LEN);
  });
});

describe('titleToCommit', () => {
  it('sends a trimmed, changed title', () => {
    expect(titleToCommit('  Renamed  ', 'Before')).toBe('Renamed');
  });

  it('sends nothing when the editor was not touched', () => {
    expect(titleToCommit('Before', 'Before')).toBeNull();
    expect(titleToCommit('  Before  ', 'Before')).toBeNull();
  });

  it('sends nothing for a blank draft, so blur cannot erase a title', () => {
    expect(titleToCommit('', 'Before')).toBeNull();
    expect(titleToCommit('   ', 'Before')).toBeNull();
    expect(titleToCommit('\n\t ', 'Before')).toBeNull();
  });

  it('sends nothing when an untitled row is blurred on its seeded preview', () => {
    const seed = seedTitleDraft({ last_user_text: 'what is a monad' });
    expect(titleToCommit(seed, seed)).toBeNull();
  });
});
