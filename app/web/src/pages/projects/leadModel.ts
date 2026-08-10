import type { LeadConversation, LeadTurn } from './api';

/// One thing the panel draws, in the order it happened.
///
/// The panel used to know only two shapes — a message and an italic line —
/// which is why the lead's tool calls arrived as prose with no link back to
/// the card they created. `event` is that third shape.
export type LeadItem =
  | { kind: 'message'; id: string; role: string; text: string; row: LeadTurn }
  | { kind: 'work'; id: string; row: LeadTurn }
  | { kind: 'event'; id: string; action: LeadAction; issues: number[]; count: number };

/// What the lead did to the board, as the inline card announces it. Only
/// the two board-shaped tool calls get a card: the rest of a turn's
/// machinery belongs in the work block.
export type LeadAction = 'created' | 'assigned' | 'updated';

/// A `Map`, not an index signature: the key is an arbitrary tool name off
/// the wire, and a `Record<string, …>` would type every miss as a hit.
const TOOL_ACTION = new Map<string, LeadAction>([
  ['IssueCreate', 'created'],
  ['IssueUpdate', 'updated'],
]);

export const ACTION_LABEL: Record<LeadAction, string> = {
  created: 'created',
  assigned: 'assigned',
  updated: 'updated',
};

type Step = {
  kind?: string;
  tool?: string | null;
  tool_summary?: string | null;
  text?: string | null;
};

/// Which issue numbers a tool step touched. Read off the step's text
/// because that is the only place the call's subject survives into the
/// transcript — a `#12` in the rendered line is the link the mockup's
/// event card offers, and a step that names none simply gets no link.
function issuesIn(text: string | null | undefined): number[] {
  if (text == null) return [];
  const found = [...text.matchAll(/#(\d+)/g)].map((match) => Number(match[1]));
  return [...new Set(found)].filter((n) => Number.isSafeInteger(n) && n > 0);
}

/// Turn one transcript row's steps into the board-shaped event cards it
/// earned. Consecutive calls of the same kind aggregate, which is what the
/// mockup's `×2` is: three cards for three `IssueCreate`s would bury the
/// sentence they belong to.
export function eventCards(row: LeadTurn): LeadItem[] {
  const steps = (row.steps ?? []) as Step[];
  const out: LeadItem[] = [];
  for (const step of steps) {
    const tool = step.tool ?? '';
    const action = TOOL_ACTION.get(tool);
    if (action === undefined) continue;
    // The summary is where a tool call's subject lands; the raw text is
    // the fallback for a step rendered without one.
    const issues = issuesIn(step.tool_summary ?? step.text);
    const tail = out.length > 0 ? out[out.length - 1] : undefined;
    if (tail !== undefined && tail.kind === 'event' && tail.action === action) {
      tail.count += 1;
      tail.issues = [...new Set([...tail.issues, ...issues])];
      continue;
    }
    out.push({
      kind: 'event',
      id: `${row.id}-event-${String(out.length)}`,
      action,
      issues,
      count: 1,
    });
  }
  return out;
}

/// The panel's render list for one page of transcript.
export function leadItems(rows: LeadTurn[]): LeadItem[] {
  const out: LeadItem[] = [];
  for (const row of rows) {
    if (row.kind === 'work') {
      out.push({ kind: 'work', id: row.id, row });
      out.push(...eventCards(row));
      continue;
    }
    out.push({ kind: 'message', id: row.id, role: row.role, text: row.text, row });
  }
  return out;
}

/// Merge a live row into the list already on screen without repeating it.
///
/// The panel appends optimistically when the operator sends, and the socket
/// echoes the same message back — so without this the sender saw their own
/// line twice. Matching on ordinal when there is one and on content when
/// there is not is what tells "my message came back" from "somebody else
/// said the same thing", which is the only case the fallback can confuse.
export function mergeLiveRow(current: LeadTurn[], incoming: LeadTurn): LeadTurn[] {
  const byOrdinal =
    incoming.ordinal == null
      ? -1
      : current.findIndex((row) => row.ordinal != null && row.ordinal === incoming.ordinal);
  if (byOrdinal !== -1) {
    const next = [...current];
    next[byOrdinal] = incoming;
    return next;
  }
  if (incoming.role === 'user') {
    // The optimistic row has no ordinal yet; the echo replaces it in place
    // so the message does not jump position as it settles.
    const pending = current.findIndex(
      (row) => row.role === 'user' && row.ordinal == null && row.text === incoming.text,
    );
    if (pending !== -1) {
      const next = [...current];
      next[pending] = incoming;
      return next;
    }
  }
  return [...current, incoming];
}

/// What the history picker shows for a conversation the auto-titler has not
/// named yet. Lead sessions are excluded from auto-titling, so this is the
/// common case rather than the fallback.
export function conversationLabel(
  conversation: LeadConversation,
  index: number,
  total: number,
): string {
  if (conversation.title != null && conversation.title.length > 0) return conversation.title;
  return `Conversation ${String(total - index)}`;
}

/// The gateway's wording when a board has no `@lead` to bind a conversation
/// to. Matched rather than typed because it is a 404 message, and a board
/// in this state is a **legacy** one: every project created since the lead
/// is seeded with the board, so this cannot happen to a new one.
///
/// Deliberately not repaired behind the operator's back. This repo does not
/// write legacy-data migrations, and silently minting an agent on somebody's
/// board is exactly the kind of background fix that rule exists to prevent —
/// so the panel explains the state and offers the ordinary hire form.
export function isMissingLead(message: string): boolean {
  return /has no lead/i.test(message);
}
