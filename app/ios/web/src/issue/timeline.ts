import type { IssueEvent } from "./types";


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
  return ALWAYS_SHOWN.has(event.body.kind);
}

export function fold(events: IssueEvent[], breakBefore?: string): Fold[] {
  const out: Fold[] = [];
  for (const event of events) {
    if (isAlwaysShown(event)) {
      out.push({ kind: "entry", event });
      continue;
    }
    const last = out.length > 0 ? out[out.length - 1] : undefined;
    if (last?.kind === "system" && event.id !== breakBefore) {
      last.events = [...last.events, event];
    } else {
      out.push({ kind: "system", events: [event] });
    }
  }
  return out;
}

/// The row a `Fold` is drawn at, which is what the unread rule anchors to.
export function foldHead(item: Fold): IssueEvent | undefined {
  return item.kind === "entry" ? item.event : item.events[0];
}

export function pendingApprovals(events: IssueEvent[]): IssueEvent[] {
  // This is a historical replay only; native separately gates controls with
  // the live approval_pending bit so cached requests cannot be answered.
  const open = new Map<string, IssueEvent>();
  for (const event of events) {
    const callId = typeof event.body.call_id === "string" ? event.body.call_id : null;
    if (callId === null) continue;
    if (event.body.kind === "approval_requested") open.set(callId, event);
    else if (event.body.kind === "approval_resolved") open.delete(callId);
  }
  return [...open.values()];
}

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
