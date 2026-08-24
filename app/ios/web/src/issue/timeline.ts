import type { IssueEvent } from "./types";

/// A card's Activity, folded for a phone.
///
/// Most of what a card records is machinery — moved, assigned, run started,
/// run settled, worktree reclaimed — and on a screen this narrow that buries
/// the two things a person actually said. Consecutive machinery entries
/// collapse into one expandable row; **comments, approvals and blocks never
/// do**, because they are the reason anybody opened the card.
///
/// Mirrors `app/ios`'s `IssueTimeline.fold`, which the Waiting strip reads for
/// the same timeline. The two agree on which kinds are always shown; nothing
/// enforces that but this comment and the tests on either side.

/// Kinds that stand alone. A person said it, a person must answer it, or it
/// stopped the card — none of those is machinery.
const ALWAYS_SHOWN = new Set([
  "comment",
  "approval_requested",
  "approval_resolved",
  "blocked",
  "unblocked",
]);

export type Fold =
  | { kind: "entry"; event: IssueEvent }
  /// One or more consecutive machinery entries, oldest first.
  | { kind: "system"; events: IssueEvent[] };

export function isAlwaysShown(event: IssueEvent): boolean {
  // An agent's own comment is a comment whoever wrote it; the kind decides,
  // not the actor. The actor matters only for a BLOCK, and that distinction
  // lives in the Waiting strip rather than here — this decides what folds.
  return ALWAYS_SHOWN.has(event.body.kind);
}

export function fold(events: IssueEvent[]): Fold[] {
  const out: Fold[] = [];
  for (const event of events) {
    if (isAlwaysShown(event)) {
      out.push({ kind: "entry", event });
      continue;
    }
    // Guarded on length rather than by optional-chaining an indexed read:
    // without `noUncheckedIndexedAccess`, `out[out.length - 1]` is typed as
    // present even on an empty array, so the `?.` the empty case actually
    // needs reads to the linter as dead code.
    const last = out.length > 0 ? out[out.length - 1] : undefined;
    if (last?.kind === "system") {
      last.events = [...last.events, event];
    } else {
      out.push({ kind: "system", events: [event] });
    }
  }
  return out;
}

/// Prompts requested and not yet resolved, oldest first.
///
/// A replay rather than a scan for the newest: a card can hold several across
/// one run, a resolution retires exactly one of them by `call_id`, and the
/// same id can be asked twice in a run.
///
/// **The live queue is the truth, not this.** A gateway restart drops every
/// parked prompt without writing a resolution, and a timed-out prompt
/// self-denies the same way — so an entry surviving here can name a prompt
/// nothing is waiting for, which is why answering one tolerates a 404.
export function pendingApprovals(events: IssueEvent[]): IssueEvent[] {
  const open = new Map<string, IssueEvent>();
  for (const event of events) {
    const callId = typeof event.body.call_id === "string" ? event.body.call_id : null;
    if (callId === null) continue;
    if (event.body.kind === "approval_requested") open.set(callId, event);
    else if (event.body.kind === "approval_resolved") open.delete(callId);
  }
  return [...open.values()];
}

/// The block in force, if an AGENT wrote it.
///
/// An operator's own block is not a question — nothing should invite somebody
/// to answer themselves — and the newest block is the one that counts, because
/// an earlier one may have been lifted and re-applied by somebody else.
export function agentQuestion(
  blockedReason: string | undefined,
  events: IssueEvent[],
): { askedBy: string; question: string } | null {
  if (blockedReason === undefined || blockedReason === "") return null;
  for (const event of [...events].reverse()) {
    if (event.body.kind !== "blocked") continue;
    if (event.actor.kind !== "agent") return null;
    return { askedBy: event.actor.handle, question: blockedReason };
  }
  return null;
}
