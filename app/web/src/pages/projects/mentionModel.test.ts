import { describe, expect, it } from 'vitest';

import type { Agent } from './boardModel';
import { applyMention, mentionCandidates, mentionQuery } from './mentionModel';

function agent(handle: string): Agent {
  return {
    id: `id-${handle}`,
    handle,
    name: handle,
    description: '',
    framework: 'baybo',
    lead: false,
    created_at_ms: 0,
  };
}

const TEAM = [agent('lead'), agent('dev-1'), agent('dev-2')];

describe('mentionQuery', () => {
  it('offers a completion while a handle is being typed', () => {
    expect(mentionQuery('@de', 3)).toEqual({ start: 0, prefix: 'de' });
    expect(mentionQuery('ask @de', 7)).toEqual({ start: 4, prefix: 'de' });
    expect(mentionQuery('@', 1)).toEqual({ start: 0, prefix: '' });
  });

  it('does not offer inside something that is not a mention', () => {
    expect(mentionQuery('me@dev', 6)).toBeNull();
    expect(mentionQuery('docs/x@lead', 11)).toBeNull();
    expect(mentionQuery('no handle here', 5)).toBeNull();
  });

  it('stops offering once the handle is finished', () => {
    expect(mentionQuery('@dev-1 please', 13)).toBeNull();
  });
});

describe('mentionCandidates', () => {
  it('narrows by prefix and offers everyone for a bare @', () => {
    expect(mentionCandidates(TEAM, 'dev').map((a) => a.handle)).toEqual(['dev-1', 'dev-2']);
    expect(mentionCandidates(TEAM, '')).toHaveLength(3);
    expect(mentionCandidates(TEAM, 'zz')).toHaveLength(0);
  });
});

describe('applyMention', () => {
  it('replaces what was typed and leaves the caret one past the mention', () => {
    const cases = [
      {
        text: 'ask @de about it',
        query: { start: 4, prefix: 'de' },
        want: 'ask @dev-1 about it',
        caretAt: 'ask @dev-1 ',
      },
      {
        text: 'ask @de,',
        query: { start: 4, prefix: 'de' },
        want: 'ask @dev-1 ,',
        caretAt: 'ask @dev-1 ',
      },
      {
        text: 'ask @de',
        query: { start: 4, prefix: 'de' },
        want: 'ask @dev-1 ',
        caretAt: 'ask @dev-1 ',
      },
      {
        text: '@de',
        query: { start: 0, prefix: 'de' },
        want: '@dev-1 ',
        caretAt: '@dev-1 ',
      },
    ];

    for (const { text, query, want, caretAt } of cases) {
      const result = applyMention(text, query, 'dev-1');
      expect(result.text).toBe(want);
      expect(result.text.slice(0, result.caret)).toBe(caretAt);
      expect(result.caret).toBe(caretAt.length);
    }
  });
});

