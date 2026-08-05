import type { components } from '../../api/schema';

import { COLUMN_LABEL } from './boardModel';

export type IssueEvent = components['schemas']['IssueEventDto'];
export type IssueEventBody = components['schemas']['IssueEventBodyDto'];

/**
 * How an entry reads. Comments are quoted material and get their own
 * block; everything else is a one-line note about what happened.
 */
export type EventShape = 'comment' | 'note';

export function eventShape(event: IssueEvent): EventShape {
  return event.body.kind === 'comment' ? 'comment' : 'note';
}

/** Who to show. The operator is "you"; an agent is its own handle. */
export function actorLabel(event: IssueEvent): string {
  return event.actor_is_agent ? `@${event.actor}` : 'you';
}

/**
 * The sentence a system entry reads as, in third person and without its
 * actor — the caller renders the actor, so putting it here too would
 * produce "you you moved this".
 *
 * Returns `null` for a comment: a comment is not narrated, it is shown.
 */
export function describeEvent(body: IssueEventBody): string | null {
  switch (body.kind) {
    case 'comment':
      return null;
    case 'opened':
      return 'opened this issue';
    case 'moved':
      return `moved it from ${COLUMN_LABEL[body.from]} to ${COLUMN_LABEL[body.to]}`;
    case 'assigned': {
      // Four cases, and each one reads differently. "assigned it to nobody"
      // is what a naive template produces for an unassignment.
      const from = body.from != null ? `@${body.from}` : null;
      const to = body.to != null ? `@${body.to}` : null;
      if (to == null) return from != null ? `unassigned ${from}` : 'unassigned it';
      if (from == null) return `assigned it to ${to}`;
      return `reassigned it from ${from} to ${to}`;
    }
    case 'run_started':
      return `started run #${body.attempt} (${body.trigger})`;
    case 'run_settled': {
      const detail = body.error != null && body.error.length > 0 ? ` — ${body.error}` : '';
      return `run #${body.attempt} ${body.status}${detail}`;
    }
    case 'blocked':
      return `blocked it: ${body.reason}`;
    case 'unblocked':
      return 'unblocked it';
    case 'cancelled':
      return 'cancelled it';
  }
}

/**
 * What sending a comment will do, stated before it is sent.
 *
 * The rule is the spec's: a comment on live work reaches the assignee, a
 * comment on an unassigned issue or on work that is parked only records.
 * The hint exists because those look identical in the composer, and a
 * person who believes they are talking to an agent when they are talking
 * to a log will wait for an answer that is never coming.
 *
 * Delivery itself is not built yet, so today every branch records. The
 * hint says so rather than promising a wake that will not happen.
 */
export function commentHint(issue: {
  status: components['schemas']['IssueDto']['status'];
  assignee?: string | null;
}): string {
  if (issue.assignee == null) {
    return 'Records only — nobody is assigned to this issue yet.';
  }
  if (issue.status === 'backlog' || issue.status === 'done') {
    return `Records only — @${issue.assignee} is not working on this right now.`;
  }
  return `Records for now. Waking @${issue.assignee} on comment is not built yet.`;
}

/** Short clock for a timeline row: today shows a time, older shows a date. */
export function eventTime(atMs: number, nowMs: number): string {
  const at = new Date(atMs);
  const sameDay = new Date(nowMs).toDateString() === at.toDateString();
  return sameDay
    ? at.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
    : at.toLocaleDateString([], { month: 'short', day: 'numeric' });
}
