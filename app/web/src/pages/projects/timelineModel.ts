import type { components } from '../../api/schema';

import { COLUMN_LABEL, type IssueStatus, type RunStatus } from './boardModel';

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
 * A deliberate mirror of `comment_delivery` in `crates/project`: the
 * composer has to say what will happen *before* the request, so it cannot
 * ask the server. The two must agree, and the pair of test suites is what
 * holds them together — if the rule changes on one side, change it here.
 *
 * The hint exists because these outcomes look identical in a text box, and
 * a person who believes an agent is reading them will wait for an answer
 * nobody is sending.
 */
export function commentHint(
  issue: { status: IssueStatus; assignee?: string | null; cancelled_at_ms?: number | null },
  runs: { status: RunStatus }[],
): string {
  const assignee = issue.assignee;
  if (assignee == null) {
    return 'Records only — nobody is assigned to this issue yet.';
  }
  if (issue.cancelled_at_ms != null) {
    return 'Records only — this issue is cancelled.';
  }
  if (issue.status === 'backlog' || issue.status === 'done') {
    return `Records only — @${assignee} is not working on this right now.`;
  }
  const live = runs.find((run) => run.status === 'queued' || run.status === 'running');
  if (live?.status === 'queued') {
    return `@${assignee} will read this when the queued run starts.`;
  }
  if (live?.status === 'running') {
    return `@${assignee} is mid-run — this is picked up when that run finishes.`;
  }
  return `Starts a run: @${assignee} will read this now.`;
}

/** Short clock for a timeline row: today shows a time, older shows a date. */
export function eventTime(atMs: number, nowMs: number): string {
  const at = new Date(atMs);
  const sameDay = new Date(nowMs).toDateString() === at.toDateString();
  return sameDay
    ? at.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
    : at.toLocaleDateString([], { month: 'short', day: 'numeric' });
}
