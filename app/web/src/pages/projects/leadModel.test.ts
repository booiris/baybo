import { describe, expect, it } from 'vitest';

import type { LeadTurn } from './api';
import { conversationLabel, eventCards, leadItems, mergeLiveRow } from './leadModel';

function row(over: Partial<LeadTurn> = {}): LeadTurn {
  return {
    id: 'r1',
    kind: 'message',
    role: 'assistant',
    text: '',
    created_at: '',
    has_attachments: false,
    ...over,
  } as LeadTurn;
}

describe('mergeLiveRow', () => {
  it('replaces the sender’s optimistic row instead of repeating it', () => {
    // The panel appends when you send and the socket echoes the same
    // message back. Without this the sender saw their own line twice.
    const optimistic = row({ id: 'local-1', role: 'user', text: 'plan it', ordinal: null });
    const echo = row({ id: 'server-1', role: 'user', text: 'plan it', ordinal: 7 });

    const merged = mergeLiveRow([optimistic], echo);
    expect(merged).toHaveLength(1);
    expect(merged[0].id).toBe('server-1');
  });

  it('keeps a second, genuinely new message from the same sender', () => {
    const first = row({ id: 'server-1', role: 'user', text: 'plan it', ordinal: 7 });
    const second = row({ id: 'server-2', role: 'user', text: 'and stage it', ordinal: 8 });
    expect(mergeLiveRow([first], second)).toHaveLength(2);
  });

  it('replaces a row that arrives twice on the same ordinal', () => {
    const once = row({ id: 'a', ordinal: 4, text: 'partial' });
    const again = row({ id: 'a', ordinal: 4, text: 'complete' });
    const merged = mergeLiveRow([once], again);
    expect(merged).toHaveLength(1);
    expect(merged[0].text).toBe('complete');
  });

  it('appends an assistant reply rather than matching it against a draft', () => {
    const mine = row({ id: 'local-1', role: 'user', text: 'same words', ordinal: null });
    const theirs = row({ id: 'server-1', role: 'assistant', text: 'same words', ordinal: 3 });
    expect(mergeLiveRow([mine], theirs)).toHaveLength(2);
  });
});

describe('eventCards', () => {
  it('aggregates consecutive calls of one kind', () => {
    const card = eventCards(
      row({
        kind: 'work',
        steps: [
          { kind: 'tool', tool: 'IssueCreate', tool_summary: 'opened #16' },
          { kind: 'tool', tool: 'IssueCreate', tool_summary: 'opened #17' },
        ],
      } as Partial<LeadTurn>),
    );
    expect(card).toHaveLength(1);
    expect(card[0]).toMatchObject({ kind: 'event', action: 'created', count: 2 });
  });

  it('separates runs of different kinds', () => {
    const cards = eventCards(
      row({
        kind: 'work',
        steps: [
          { kind: 'tool', tool: 'IssueCreate', tool_summary: 'opened #16' },
          { kind: 'tool', tool: 'IssueUpdate', tool_summary: 'assigned #16' },
          { kind: 'tool', tool: 'IssueCreate', tool_summary: 'opened #18' },
        ],
      } as Partial<LeadTurn>),
    );
    expect(cards.map((c) => (c.kind === 'event' ? c.action : ''))).toEqual([
      'created',
      'updated',
      'created',
    ]);
  });

  it('ignores machinery that did not touch the board', () => {
    // A card is the lead's *board* actions; the rest belongs in the work
    // block, where it does not compete with the sentence it supports.
    const cards = eventCards(
      row({
        kind: 'work',
        steps: [
          { kind: 'tool', tool: 'IssueList', tool_summary: 'read the board' },
          { kind: 'reasoning', text: 'thinking' },
        ],
      } as Partial<LeadTurn>),
    );
    expect(cards).toHaveLength(0);
  });
});

describe('leadItems', () => {
  it('puts a turn’s event cards under its work block', () => {
    const items = leadItems([
      row({
        id: 'w',
        kind: 'work',
        steps: [{ kind: 'tool', tool: 'IssueCreate', tool_summary: 'opened #4' }],
      } as Partial<LeadTurn>),
      row({ id: 'm', kind: 'message', role: 'assistant', text: 'done' }),
    ]);
    expect(items.map((i) => i.kind)).toEqual(['work', 'event', 'message']);
  });
});

describe('conversationLabel', () => {
  it('prefers the auto title and falls back to a stable ordinal', () => {
    const titled = { session_id: 'a', last_active_ms: 0, created_at_ms: 0, title: 'Budget talk' };
    const bare = { session_id: 'b', last_active_ms: 0, created_at_ms: 0 };
    expect(conversationLabel(titled as never, 0, 2)).toBe('Budget talk');
    // Lead sessions are excluded from auto-titling, so this is the common
    // case, not the edge one — the number counts down from the newest.
    expect(conversationLabel(bare as never, 1, 2)).toBe('Conversation 1');
  });
});
