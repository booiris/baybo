import type { Agent } from './boardModel';

export type MentionQuery = { start: number; prefix: string };

export function mentionQuery(text: string, caret: number): MentionQuery | null {
  const before = text.slice(0, caret);
  const at = before.lastIndexOf('@');
  if (at === -1) return null;
  const preceding = at === 0 ? '' : before[at - 1];
  if (preceding !== '' && !/[\s(]/.test(preceding)) return null;
  const prefix = before.slice(at + 1);
  if (!/^[a-z0-9-]*$/.test(prefix)) return null;
  return { start: at, prefix };
}

export function mentionCandidates(team: Agent[], prefix: string): Agent[] {
  const needle = prefix.toLowerCase();
  return team.filter((agent) => agent.handle.startsWith(needle));
}

export function applyMention(
  text: string,
  query: MentionQuery,
  handle: string,
): { text: string; caret: number } {
  const head = text.slice(0, query.start);
  const tail = text.slice(query.start + 1 + query.prefix.length);
  const mention = `@${handle}`;
  const inserted = /^\s/.test(tail) ? mention : `${mention} `;
  return { text: `${head}${inserted}${tail}`, caret: head.length + mention.length + 1 };
}

/// Describe a draft mention, including the blocked-card refusal.
export function mentionHint(
  issue: { assignee?: string | null; blocked_reason?: string | null },
  text: string,
  team: Agent[],
): string | null {
  if (issue.assignee != null) return null;
  const first = /(^|[\s(])@([a-z0-9-]+)/.exec(text);
  if (first === null) return null;
  const handle = first[2].replace(/-+$/, '');
  if (!team.some((agent) => agent.handle === handle)) return null;
  if (issue.blocked_reason != null) {
    return `Records only — a block has stopped this issue; @${handle} is not put on it until it is lifted.`;
  }
  return `Sending this puts @${handle} on the issue.`;
}
