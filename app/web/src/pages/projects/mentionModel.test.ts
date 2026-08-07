import { describe, expect, it } from 'vitest';

import type { Agent } from './boardModel';
import { applyMention, mentionCandidates, mentionHint, mentionQuery } from './mentionModel';

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
    // A bare `@` offers the whole team.
    expect(mentionQuery('@', 1)).toEqual({ start: 0, prefix: '' });
  });

  it('does not offer inside something that is not a mention', () => {
    // Same rule the server applies, so the composer cannot promise a
    // handover the server will not make.
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
      // Mid-sentence: the tail's space is reused, and the caret steps over it.
      {
        text: 'ask @de about it',
        query: { start: 4, prefix: 'de' },
        want: 'ask @dev-1 about it',
        caretAt: 'ask @dev-1 ',
      },
      // Before punctuation: the inserted space is the separator, so the caret
      // belongs before the comma, not after it.
      {
        text: 'ask @de,',
        query: { start: 4, prefix: 'de' },
        want: 'ask @dev-1 ,',
        caretAt: 'ask @dev-1 ',
      },
      // At the end of the box, where the caret must not run past the text.
      {
        text: 'ask @de',
        query: { start: 4, prefix: 'de' },
        want: 'ask @dev-1 ',
        caretAt: 'ask @dev-1 ',
      },
      // The mention is the whole comment.
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
      // The offset itself, not just the prefix: `slice` clamps, which is how
      // a caret past the end of the text stayed invisible.
      expect(result.caret).toBe(caretAt.length);
    }
  });
});

describe('mentionHint', () => {
  it('promises a handover only on a card nobody is on', () => {
    expect(mentionHint({ assignee: null }, '@dev-1 take this', TEAM)).toBe(
      'Sending this puts @dev-1 on the issue.',
    );
    // On an owned card a mention is a question, so promising a handover
    // would be promising something the server will not do.
    expect(mentionHint({ assignee: 'id-dev-2' }, '@dev-1 take this', TEAM)).toBeNull();
  });

  it('names the first handle, which is the one that wins', () => {
    expect(mentionHint({ assignee: null }, '@dev-1 and @dev-2 look', TEAM)).toContain('@dev-1');
  });

  it('says nothing for a handle nobody has', () => {
    expect(mentionHint({ assignee: null }, '@nobody look', TEAM)).toBeNull();
    expect(mentionHint({ assignee: null }, 'no mention', TEAM)).toBeNull();
  });
});
