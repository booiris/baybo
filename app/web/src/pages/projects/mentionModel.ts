import type { Agent } from './boardModel';

/** What the composer is completing, if anything. */
export type MentionQuery = { start: number; prefix: string };

/**
 * The `@handle` being typed at `caret`, if the caret is inside one.
 *
 * Mirrors the server's mention grammar: a handle starts after whitespace
 * (or at the start), and runs while the characters are ones a handle can
 * contain. Anything else — an email address, a path — is not an offer to
 * complete.
 */
export function mentionQuery(text: string, caret: number): MentionQuery | null {
  const before = text.slice(0, caret);
  const at = before.lastIndexOf('@');
  if (at === -1) return null;
  const preceding = at === 0 ? '' : before[at - 1];
  if (preceding !== '' && !/[\s(]/.test(preceding)) return null;
  const prefix = before.slice(at + 1);
  // A space ends the handle, so the caret has left it.
  if (!/^[a-z0-9-]*$/.test(prefix)) return null;
  return { start: at, prefix };
}

/** Teammates whose handle starts with what has been typed. */
export function mentionCandidates(team: Agent[], prefix: string): Agent[] {
  const needle = prefix.toLowerCase();
  return team.filter((agent) => agent.handle.startsWith(needle));
}

/** The text and caret after accepting a completion. */
export function applyMention(
  text: string,
  query: MentionQuery,
  handle: string,
): { text: string; caret: number } {
  const head = text.slice(0, query.start);
  const tail = text.slice(query.start + 1 + query.prefix.length);
  const mention = `@${handle}`;
  // A trailing space only when the text does not already have one:
  // completing mid-sentence must not leave a double space where the caret
  // then sits.
  const inserted = /^\s/.test(tail) ? mention : `${mention} `;
  // One past the mention is one past its separator either way — the space
  // just inserted, or the one the tail already began with. Measuring from
  // `inserted` instead runs the caret off the end of the box whenever the
  // separator was the inserted one.
  return { text: `${head}${inserted}${tail}`, caret: head.length + mention.length + 1 };
}

/**
 * What a mention will do, said before the comment is sent.
 *
 * Mirrors `assigns_to` in `crates/project`: only an unowned card changes
 * hands, and only to the first handle named. On a card somebody is working
 * a mention is a question, and saying otherwise would be promising a
 * handover that will not happen.
 */
export function mentionHint(
  issue: { assignee?: string | null },
  text: string,
  team: Agent[],
): string | null {
  if (issue.assignee != null) return null;
  const first = /(^|[\s(])@([a-z0-9-]+)/.exec(text);
  if (first === null) return null;
  const handle = first[2].replace(/-+$/, '');
  if (!team.some((agent) => agent.handle === handle)) return null;
  return `Sending this puts @${handle} on the issue.`;
}
