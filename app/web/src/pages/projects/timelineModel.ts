import type { components, paths } from '../../api/schema';

import { COLUMN_LABEL, RUN_TRIGGER_LABEL, formatDuration } from './boardModel';
import { formatTokens, formatUsd } from './budgetModel';

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

/// The six colours this board reads a status in — one vocabulary, so "amber"
/// cannot mean a warning in one place and work-about-to-start in another.
export type Tone = 'ok' | 'err' | 'warn' | 'info' | 'brand' | 'muted';

/// The vocabulary as classes. Lives with `eventTone` rather than in either
/// component, because the card's rail and the board's feed are two readers of
/// one alphabet — a second copy is how amber starts meaning two things.
export const TONE_DOT: Record<Tone, string> = {
  ok: 'bg-ok',
  err: 'bg-err',
  warn: 'bg-warn',
  info: 'bg-info',
  brand: 'bg-brand-hover',
  muted: 'bg-ink-soft',
};

/// What colour an entry's dot is. The timeline is skimmed down its rail
/// before it is read, so a failure and a hire must be distinguishable
/// without parsing either sentence.
export function eventTone(body: IssueEventBody): Tone {
  switch (body.kind) {
    case 'assigned':
      return 'brand';
    case 'moved':
    case 'run_started':
      return 'info';
    case 'run_settled':
      return body.status === 'done' ? 'ok' : body.status === 'failed' ? 'err' : 'muted';
    case 'blocked':
    case 'run_interrupted':
    case 'approval_requested':
    case 'budget_exhausted':
    case 'token_budget_exhausted':
    // Warn, not muted: the write that implied this run stands on the card,
    // and the work it implied did not happen. That is the shape of a thing
    // the operator has to look at, not a thing the board did routinely.
    case 'run_refused':
      return 'warn';
    case 'unblocked':
    case 'stage_completed':
    case 'budget_restored':
    case 'token_budget_restored':
    // The card's work reaching the repository is the outcome the whole
    // board exists for, so it reads as one — not as the muted routine note
    // the `default` arm below would give it.
    case 'branch_merged':
      return 'ok';
    case 'approval_resolved':
      // How it resolved outranks what was decided: a window that expired or
      // a prompt that died with its run is nobody saying no.
      if (body.resolution === 'timed_out') return 'warn';
      if (body.resolution === 'abandoned') return 'muted';
      return body.decision === 'deny' ? 'err' : 'ok';
    default:
      return 'muted';
  }
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

/// How much of a sha a person reads before their eyes glaze over, and
/// enough for `git show` to resolve. Mirrors `SHORT_SHA` in `issue_get.rs`.
const SHORT_SHA = 8;

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

/// Every approval this card has ever asked for, open or settled, by call id.
///
/// The settled card still shows the command that was approved — "who let
/// this through" is only half the question a reader comes back with, and a
/// decision with the command stripped off answers neither half.
export function approvalAsks(events: IssueEvent[]): Map<string, PendingApproval> {
  const asks = new Map<string, PendingApproval>();
  for (const event of events) {
    if (event.body.kind !== 'approval_requested') continue;
    asks.set(event.body.call_id, {
      callId: event.body.call_id,
      tool: event.body.tool,
      summary: event.body.summary,
      attempt: event.body.attempt ?? null,
    });
  }
  return asks;
}

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
    case 'run_interrupted':
      return `run #${body.attempt} was interrupted before it finished — the board picked it up again`;
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
    case 'branch_merged': {
      // The branch it landed on is named rather than assumed: a repository
      // parked somewhere other than its trunk merges there.
      const at = body.commit ? ` as ${body.commit.slice(0, SHORT_SHA)}` : '';
      const many = body.commits === 1 ? '1 commit' : `${body.commits} commits`;
      return `merged ${body.branch} into ${body.into}${at} — ${many}`;
    }
    case 'worktree_reclaimed':
      return body.branch_deleted
        ? 'reclaimed the worktree and deleted its branch — it held nothing this repo did not already have'
        : 'reclaimed the worktree; the branch is still there';
    case 'worktree_kept':
      return `kept the worktree — ${body.reason}`;
    case 'approval_requested':
      return `asked you to approve a ${body.tool} call: ${body.summary}`;
    case 'approval_resolved':
      switch (body.resolution) {
        case 'timed_out':
          return 'nobody answered the approval in time — denied by default';
        case 'abandoned':
          return 'the approval prompt went away undecided — its run was interrupted';
        case 'policy':
          return `the ${DECISION_LABEL[body.decision]} came from standing policy, without a prompt`;
        default:
          return `the ${DECISION_LABEL[body.decision]} was recorded`;
      }
    case 'stage_completed':
      return `stage ${body.stage} finished — every step in it is done or called off`;
    case 'run_refused':
      // Named for the run holding the slot, because that is the half a
      // reader can act on: the card already shows the move or the handover
      // that was made, and this is the run it implied not happening.
      return body.attempt != null
        ? `did not start a run (${RUN_TRIGGER_LABEL[body.trigger]}) — run #${body.attempt} still has this card`
        : `did not start a run (${RUN_TRIGGER_LABEL[body.trigger]}) — this card already had one in flight`;
    case 'filed':
      return `filed #${body.number} out of this card's work`;
    case 'budget_exhausted':
      return `held the run — ${formatUsd(body.spent_micros)} of the ${formatUsd(body.limit_micros)} daily budget is spent`;
    case 'budget_restored':
      return `started the held run — ${formatUsd(body.spent_micros)} of ${formatUsd(body.limit_micros)} spent today`;
    case 'token_budget_exhausted':
      return `held the run — ${formatTokens(body.spent_tokens)} of the ${formatTokens(
        body.limit_tokens,
      )} daily token budget is spent`;
    case 'token_budget_restored':
      return `started the held run — ${formatTokens(body.spent_tokens)} of ${formatTokens(
        body.limit_tokens,
      )} tokens spent today`;
    default:
      return unnamedEvent(body);
  }
}

function unnamedEvent(_body: never): string {
  return 'did something this page is too old to describe';
}

/// When an entry happened, to the minute — always. An older entry used to
/// carry its date and no clock, which reads fine in a list until two runs a
/// morning apart sit next to each other saying the same thing. The date is
/// what drops off for today, not the time.
export function eventTime(atMs: number, nowMs: number): string {
  const at = new Date(atMs);
  const clock = at.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  const sameDay = new Date(nowMs).toDateString() === at.toDateString();
  if (sameDay) return clock;
  return `${at.toLocaleDateString([], { month: 'short', day: 'numeric' })} ${clock}`;
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

/// What colour a **feed** line's dot is. The board-level entries have no
/// `IssueEventBody` to ask, so the feed needs its own door onto the same
/// vocabulary rather than a second one beside it.
export function feedTone(entry: FeedEntry): Tone {
  if (entry.body.kind === 'hired') return 'brand';
  return eventTone(entry.body);
}

/// A run of text, and whether it is the part the eye should land on.
///
/// The feed is skimmed, so who acted, which card, and the one word that says
/// how it went are set in bold and everything joining them is not. A plain
/// string could not carry that, and marking it up in the component would put
/// the sentence in two places.
export type Span = { text: string; strong?: boolean };

function who(entry: FeedEntry): Span[] {
  return [{ text: feedActorLabel(entry), strong: true }];
}

function card(number: number | null | undefined): Span[] {
  return number == null ? [] : [{ text: `#${number}`, strong: true }];
}

function join(...parts: (Span[] | Span | string)[]): Span[] {
  const spans: Span[] = [];
  for (const part of parts) {
    if (typeof part === 'string') spans.push({ text: part });
    else if (Array.isArray(part)) spans.push(...part);
    else spans.push(part);
  }
  return spans;
}

/// What a feed line says.
///
/// Not `describeEvent` with the actor bolted on the front. That one writes
/// for a card's own timeline, where "it" is unambiguous because the whole
/// pane is one card — in a board-wide feed "moved it to Review" names
/// nothing. Every line here names its card.
///
/// A comment's own words are deliberately not the line either: an agent's run
/// report is hundreds of words that would bury every line around it. The
/// card's timeline is where the text belongs.
export function feedLine(entry: FeedEntry): Span[] {
  const body = entry.body;
  const at = card(entry.number);
  switch (body.kind) {
    case 'hired':
      return join(who(entry), ' hired ', { text: `@${body.agent.handle}`, strong: true });
    case 'comment':
      return join(who(entry), ' commented on ', at);
    case 'opened':
      return join(who(entry), ' opened ', at);
    case 'moved':
      return join(
        who(entry),
        ' moved ',
        at,
        ` ${COLUMN_LABEL[body.from]} → ${COLUMN_LABEL[body.to]}`,
      );
    case 'assigned': {
      const to = body.to;
      if (to == null) return join(who(entry), ' unassigned ', at);
      return join(who(entry), ' assigned ', { text: `@${to.handle}`, strong: true }, ' → ', at);
    }
    // The actor on a run entry is the run's **own** agent — `start_run` and
    // the settle both record it that way — and it is the only place the feed
    // can say who did the work. Left off, every run line in a board-wide feed
    // was anonymous while the `assigned` line right above it named somebody,
    // so the feed read as if the assignee had run it. On a board where a
    // review handover is a reassignment, that is usually the wrong agent.
    case 'run_started':
      return join(who(entry), `'s run #${body.attempt} started on `, at);
    // The actor here is always the board, so it is left off: what the line
    // has to name is the card and the run that took its slot.
    case 'run_refused':
      return join(
        at,
        ` did not start a run (${RUN_TRIGGER_LABEL[body.trigger]})`,
        body.attempt != null ? ` — run #${body.attempt} still has it` : ' — it already had one',
      );
    case 'run_settled':
      return join(
        who(entry),
        `'s run #${body.attempt} `,
        { text: body.status, strong: true },
        ' on ',
        at,
        // Derived server-side over the run's own cost window, so the feed
        // and the execution log cannot price the same run differently.
        entry.duration_ms == null ? '' : ` · ${formatDuration(entry.duration_ms)}`,
        entry.cost_micros == null ? '' : ` · ${formatUsd(entry.cost_micros)}`,
        body.error != null && body.error.length > 0 ? ` — ${body.error}` : '',
      );
    case 'blocked':
      return join(who(entry), ' blocked ', at, `: ${body.reason}`);
    case 'unblocked':
      return join(who(entry), ' unblocked ', at);
    case 'cancelled':
      return join(who(entry), ' cancelled ', at);
    case 'approval_requested':
      return join('approval waiting on ', at, `: ${body.tool} — ${body.summary}`);
    case 'approval_resolved':
      if (body.resolution === 'timed_out') return join('approval timed out on ', at);
      if (body.resolution === 'abandoned') return join('approval abandoned on ', at);
      return join(`approval ${DECISION_LABEL[body.decision]} on `, at);
    case 'stage_completed':
      return join(`stage ${body.stage} complete on `, at);
    case 'filed':
      return join(who(entry), ' filed ', card(body.number), ' out of ', at);
    case 'budget_exhausted':
      return join(
        'daily budget exhausted — ',
        `${formatUsd(body.spent_micros)} of ${formatUsd(body.limit_micros)} spent`,
      );
    case 'budget_restored':
      return join('budget restored — ', at, ' released');
    case 'token_budget_exhausted':
      return join(
        'daily token budget exhausted — ',
        `${formatTokens(body.spent_tokens)} of ${formatTokens(body.limit_tokens)} spent`,
      );
    case 'token_budget_restored':
      return join('token budget restored — ', at, ' released');
    default: {
      const said = describeEvent(body);
      return join(who(entry), ' · ', said ?? 'acted', at.length > 0 ? ' · ' : '', at);
    }
  }
}
