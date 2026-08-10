import type { components, paths } from '../../api/schema';

import { COLUMN_LABEL, unsettledRun, type Agent, type IssueStatus, type RunStatus } from './boardModel';
import { handleOf } from './teamModel';

export type IssueEvent = components['schemas']['IssueEventDto'];
export type IssueEventBody = components['schemas']['IssueEventBodyDto'];
/// One line of the board's activity feed. Shaped like a timeline entry, but
/// `number` is optional: a hire is a fact about the board, so there is no
/// card for it to point at.
export type FeedEntry =
  paths['/v1/projects/{project_id}/feed']['get']['responses'][200]['content']['application/json']['items'][number];

export type EventShape = 'comment' | 'note';

export function eventShape(event: IssueEvent): EventShape {
  return event.body.kind === 'comment' ? 'comment' : 'note';
}

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

function unnamedActor(_kind: never): string {
  return 'somebody';
}

function usd(micros: number): string {
  return `$${(micros / 1_000_000).toFixed(2)}`;
}

const DECISION_LABEL: Record<'approve' | 'approve_always' | 'deny', string> = {
  approve: 'approval',
  approve_always: 'standing approval',
  deny: 'refusal',
};

export type PendingApproval = {
  callId: string;
  tool: string;
  summary: string;
  /// Which run is parked. Absent on prompts recorded before the card
  /// tracked it — the card still says a tool is waiting.
  attempt: number | null;
};

export function pendingApprovals(events: IssueEvent[]): PendingApproval[] {
  const open = new Map<string, PendingApproval>();
  for (const event of events) {
    if (event.body.kind === 'approval_requested') {
      open.set(event.body.call_id, {
        callId: event.body.call_id,
        tool: event.body.tool,
        summary: event.body.summary,
        attempt: event.body.attempt ?? null,
      });
    } else if (event.body.kind === 'approval_resolved') {
      open.delete(event.body.call_id);
    }
  }
  return [...open.values()];
}

export function describeEvent(body: IssueEventBody): string | null {
  switch (body.kind) {
    case 'comment':
      return null;
    case 'opened':
      return 'opened this issue';
    case 'moved':
      return `moved it from ${COLUMN_LABEL[body.from]} to ${COLUMN_LABEL[body.to]}`;
    case 'assigned': {
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
      return body.branch_deleted
        ? 'reclaimed the worktree and deleted its branch — it held nothing this repo did not already have'
        : 'reclaimed the worktree; the branch is still there';
    case 'worktree_kept':
      return `kept the worktree — ${body.reason}`;
    case 'approval_requested':
      return `asked you to approve a ${body.tool} call: ${body.summary}`;
    case 'approval_resolved':
      return `the ${DECISION_LABEL[body.decision]} was recorded`;
    case 'stage_completed':
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

function unnamedEvent(_body: never): string {
  return 'did something this page is too old to describe';
}

/// What sending will do, in the assignee's own name.
///
/// `team` is required rather than optional: the assignee on an issue is an
/// agent **id**, and every sentence below addresses a person. Resolving it
/// here — where the hint is written — is what stops a raw ULID reaching the
/// composer, which is what happened while this took only the issue.
export function commentHint(
  issue: { status: IssueStatus; assignee?: string | null; cancelled_at_ms?: number | null },
  runs: { status: RunStatus }[],
  team: Agent[],
): string {
  const assignee = issue.assignee == null ? null : handleOf(team, issue.assignee);
  if (assignee == null) {
    return 'Records only — nobody is assigned to this issue yet.';
  }
  if (issue.cancelled_at_ms != null) {
    return 'Records only — this issue is cancelled.';
  }
  if (issue.status === 'backlog' || issue.status === 'done') {
    return `Records only — @${assignee} is not working on this right now.`;
  }
  const live = unsettledRun(runs);
  if (live?.status === 'held') {
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

export function eventTime(atMs: number, nowMs: number): string {
  const at = new Date(atMs);
  const sameDay = new Date(nowMs).toDateString() === at.toDateString();
  return sameDay
    ? at.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
    : at.toLocaleDateString([], { month: 'short', day: 'numeric' });
}

/// How long ago, for a feed that is skimmed rather than read. A wall clock
/// answers "when did this happen" only after the reader does the
/// subtraction themselves, and the feed's whole job is to be scannable.
export function eventAgo(atMs: number, nowMs: number): string {
  const seconds = Math.max(0, Math.round((nowMs - atMs) / 1000));
  if (seconds < 60) return 'now';
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours}h`;
  return `${Math.round(hours / 24)}d`;
}

/// Who did it, for a feed line. A hire whose `hired_by` was empty comes
/// back as the user's doing, which is also how a board's own lead reads —
/// nobody hired it, it came with the board.
export function feedActorLabel(entry: FeedEntry): string {
  switch (entry.actor.kind) {
    case 'user':
      return 'you';
    case 'system':
      return 'the board';
    case 'agent':
      return `@${entry.actor.handle}`;
    default:
      return unnamedActor(entry.actor);
  }
}

/// What a feed line says. A comment's own words are deliberately *not* the
/// line: the feed is a list of things that happened, and an agent's
/// run report is hundreds of words that would bury every line around it.
/// The card's timeline is where the text belongs.
export function describeFeedEntry(entry: FeedEntry): string {
  if (entry.body.kind === 'hired') {
    return `hired @${entry.body.agent.handle}`;
  }
  if (entry.body.kind === 'comment') return 'commented';
  return describeEvent(entry.body) ?? 'commented';
}
