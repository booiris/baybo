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

/**
 * Who to show. The operator is "you", an agent is its `@handle`, and the
 * board's own gate is "the board" — a budget hold is nobody's action, and
 * "you held the run" accuses the reader of one they did not take.
 *
 * The handle arrives resolved: an agent that has left the team is not on
 * the roster this page holds, and a timeline still names it.
 */
export function actorLabel(event: IssueEvent): string {
  switch (event.actor.kind) {
    case 'user':
      return 'you';
    case 'system':
      return 'the board';
    case 'agent':
      return `@${event.actor.handle}`;
    default:
      return unnamedActor(event.actor);
  }
}

/**
 * An actor kind this bundle has never heard of — a server newer than the
 * page it is serving, which `IssueActor` is explicitly designed to allow.
 *
 * The `never` parameter is what keeps the build-time check that a fourth
 * kind must be handled above; the string is what a stale bundle renders
 * instead of dropping `undefined` into the middle of a sentence.
 */
function unnamedActor(_kind: never): string {
  return 'somebody';
}

/** Micro-USD as the dollars a reader thinks in. */
function usd(micros: number): string {
  return `$${(micros / 1_000_000).toFixed(2)}`;
}

/**
 * What each decision reads as in a sentence. A record rather than the raw
 * wire value: "approve_always was recorded" is not a sentence, and the
 * exhaustiveness check is what will catch a fourth decision.
 */
const DECISION_LABEL: Record<'approve' | 'approve_always' | 'deny', string> = {
  approve: 'approval',
  approve_always: 'standing approval',
  deny: 'refusal',
};

/**
 * The approvals still waiting on a person, oldest first.
 *
 * Derived rather than stored, on both sides: a request with no matching
 * resolution on the same `call_id` is open here, and the server keeps no
 * pending flag on the card either. The timeline entry is also the only
 * thing on the board carrying a call id, so this pass is how a card gets
 * one to answer with at all.
 *
 * A view, not the authority. What the server answers from is the **parked
 * session**, not the timeline: `POST …/issues/{n}/approvals/{call_id}`
 * looks the call up in the live approval queue and resolves it only if the
 * session it is parked in belongs to this card. So the two can disagree: a
 * resolution whose timeline append was swallowed, or a restart that took
 * the queue with it, leaves a prompt listed here that the server answers
 * with a 404. Which is why `IssueDetailPage` never drops a prompt
 * optimistically — it refetches, so the entry disappears only once the
 * server's own resolution is on the timeline, and a refused click surfaces
 * as an error with the prompt still standing.
 */
export function pendingApprovals(
  events: IssueEvent[],
): { callId: string; tool: string; summary: string }[] {
  const open = new Map<string, { callId: string; tool: string; summary: string }>();
  for (const event of events) {
    if (event.body.kind === 'approval_requested') {
      open.set(event.body.call_id, {
        callId: event.body.call_id,
        tool: event.body.tool,
        summary: event.body.summary,
      });
    } else if (event.body.kind === 'approval_resolved') {
      open.delete(event.body.call_id);
    }
  }
  return [...open.values()];
}

/**
 * The sentence a system entry reads as, in third person and without its
 * actor — the caller renders the actor, so putting it here too would
 * produce "you you moved this".
 *
 * Returns `null` for a comment, and for nothing else: a comment is not
 * narrated, it is shown, and `Timeline`'s `Note` drops any entry whose
 * sentence is nullish.
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
      const from = body.from != null ? `@${body.from.handle}` : null;
      const to = body.to != null ? `@${body.to.handle}` : null;
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
    case 'worktree_reclaimed':
      // Not "nothing was committed": a branch the operator merged before
      // dragging the card to Done counts zero commits ahead and git agrees
      // to drop it, and its work is in the repo.
      return body.branch_deleted
        ? 'reclaimed the worktree and deleted its branch — it held nothing this repo did not already have'
        : 'reclaimed the worktree; the branch is still there';
    case 'worktree_kept':
      // git's own reason, because it is the actionable part: the operator
      // has to commit or discard before the tree can go.
      return `kept the worktree — ${body.reason}`;
    case 'approval_requested':
      return `asked you to approve a ${body.tool} call: ${body.summary}`;
    case 'approval_resolved':
      return `the ${DECISION_LABEL[body.decision]} was recorded`;
    case 'stage_completed':
      // A fact about the stage, and nothing about a wake: a stage that
      // empties while an earlier one is still open is announced and starts
      // nothing, so the assignee's run — when there is one — says so itself
      // as `started run #n (stage_barrier)`. Cancelled steps count as
      // finished, the same way the progress ring counts them.
      return `stage ${body.stage} finished — every step in it is done or called off`;
    case 'budget_exhausted':
      return `held the run — ${usd(body.spent_micros)} of the ${usd(
        body.limit_micros,
      )} daily budget is spent`;
    case 'budget_restored':
      return `started the held run — ${usd(body.spent_micros)} of ${usd(
        body.limit_micros,
      )} spent today`;
    default:
      return unnamedEvent(body);
  }
}

/**
 * A body kind this bundle has never heard of — the same case `actorLabel`
 * covers, on the union that grows fastest: every new thing the board learns
 * to record adds a variant here.
 *
 * The `never` parameter keeps the build-time check that a new kind must be
 * handled above. The string carries more weight than `actorLabel`'s does:
 * `Note` renders nothing at all when the sentence is nullish, so falling
 * through would delete the entry from the card's history rather than
 * degrade it, and `ActivityDrawer` would show a blank row.
 */
function unnamedEvent(_body: never): string {
  return 'did something this page is too old to describe';
}

/**
 * The run states that are over. Derived here rather than listed at each
 * call site, so a new unsettled state cannot be quietly read as finished.
 */
const SETTLED: ReadonlySet<RunStatus> = new Set<RunStatus>(['done', 'failed', 'cancelled']);

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
  const live = runs.find((run) => !SETTLED.has(run.status));
  if (live?.status === 'held') {
    // A held run has not assembled its brief either, so this lands in it —
    // the wait is on the board's budget, not on the comment.
    return `@${assignee} will read this when the held run starts — the project is over its daily budget.`;
  }
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
