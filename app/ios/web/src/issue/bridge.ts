import { postToNative } from "../bridge";
import type { ChildIssue, IssueDetail, IssueEvent, IssueRun } from "./types";

/// The card page's own half of the native bridge.
///
/// It rides the SAME `baybo` message handler as the transcript rather than
/// registering a second one, which is what lets this page import `Markdown`,
/// the attachment cards and `blobObjectUrl` unchanged — those all post through
/// that channel, and a page with its own handler would have had to fork every
/// one of them. What is here is only the messages the transcript has no idea
/// about.
///
/// Inbound arrives on `window.issuePage`, the deck's pattern. Native buffers
/// its calls until `ready`, so nothing races the first render.

export type IssueInit = {
  language: string;
  projectId: string;
  number: number;
  /// Bottom chrome (the dock, plus a ridden keyboard) at first paint, in CSS
  /// px. Streamed from then on — the webview never resizes.
  bottomInset: number;
};

/// Who an agent id is: what to call them, and what to draw for them.
///
/// The DTOs carry profile ids, and only the board knows the team — so this is
/// resolved once natively rather than in every place the page prints a name.
/// `monogram` comes with it because it is a property of the whole TEAM, not of
/// one handle (`dev-1` and `docs-1` both give `D1` until the set widens): a
/// page deriving its own would print the collision `AgentMonogram` exists to
/// avoid. `avatar` is a blob id, fetched over the same `requestBlob` bridge
/// the attachment cards use.
export type Person = { handle: string; avatar?: string; monogram: string };

export type IssuePayload = {
  issue: IssueDetail;
  events: IssueEvent[];
  runs: IssueRun[];
  /// Agent profile id → who they are. An id that resolves to nothing prints as
  /// itself, which is what the gateway does too.
  people: Record<string, Person>;
  /// This card's children, from the board. The issue DTO carries only a
  /// done/total COUNT, so listing them needs a source the card itself is not.
  children?: ChildIssue[];
  /// The entry the operator has not seen yet — `IssueTimelineDto.first_unread`,
  /// straight off the timeline response. **Resolved server-side** by the same
  /// predicate the unread badge is counted with, so this page never decides
  /// what "unread" means; it only decides where to put the rule.
  ///
  /// Absent when nothing is new, and absent while the card is painting from
  /// native's mirror — a cursor read off disk points at a boundary the server
  /// may have moved hours ago. See `IssueStore.firstUnread`.
  firstUnread?: string;
};

export type IssueEvents = {
  init(payload: IssueInit): void;
  /// A full replacement. The card is small and its parts move together — a
  /// comment writes a timeline entry AND bumps the card — so there is nothing
  /// a field-by-field merge would protect and one more way for the two to
  /// disagree if it tried.
  deliver(payload: IssuePayload): void;
  bottomInset(px: number): void;
  language(lang: string): void;
  /// The dock's ✎ / Done. Web owns the textarea; native owns the bar.
  setEditing(active: boolean): void;
  /// Scroll the newest activity into view (the "new activity" pill, and the
  /// jump native runs after a comment lands).
  jumpToLatest(): void;
};

export type IssueGlobal = {
  init(payload: IssueInit): void;
  deliver(payload: IssuePayload): void;
  setBottomInset(px: number): void;
  setLanguage(lang: string): void;
  setEditing(active: boolean): void;
  jumpToLatest(): void;
};

type Buffered =
  | { kind: "init"; payload: IssueInit }
  | { kind: "deliver"; payload: IssuePayload }
  | { kind: "bottomInset"; px: number }
  | { kind: "language"; lang: string }
  | { kind: "setEditing"; active: boolean }
  | { kind: "jumpToLatest" };

let events: IssueEvents | null = null;
const buffer: Buffered[] = [];

function dispatch(item: Buffered): void {
  if (!events) {
    buffer.push(item);
    return;
  }
  deliver(events, item);
}

function deliver(e: IssueEvents, item: Buffered): void {
  if (item.kind === "init") e.init(item.payload);
  else if (item.kind === "deliver") e.deliver(item.payload);
  else if (item.kind === "bottomInset") e.bottomInset(item.px);
  else if (item.kind === "language") e.language(item.lang);
  else if (item.kind === "setEditing") e.setEditing(item.active);
  // Every kind needs its own branch ABOVE this terminal else — it is a bare
  // fall-through to `jumpToLatest`, so a missing branch silently turns a new
  // command into "scroll to the bottom", and the type checker cannot see it.
  // `bridge.test.ts` pins the same rule for the transcript.
  else e.jumpToLatest();
}

export function subscribeIssue(e: IssueEvents): () => void {
  events = e;
  for (const item of buffer.splice(0, buffer.length)) deliver(e, item);
  return () => {
    if (events === e) events = null;
  };
}

window.issuePage = {
  init: (payload) => dispatch({ kind: "init", payload }),
  deliver: (payload) => dispatch({ kind: "deliver", payload }),
  setBottomInset: (px) => dispatch({ kind: "bottomInset", px }),
  setLanguage: (lang) => dispatch({ kind: "language", lang }),
  setEditing: (active) => dispatch({ kind: "setEditing", active }),
  jumpToLatest: () => dispatch({ kind: "jumpToLatest" }),
};

// ---- outbound --------------------------------------------------------------

export function postIssueReady(): void {
  postToNative({ type: "issueReady" });
}

/// The card rendered. Native stamps it read only now — never on the way in,
/// because a card whose timeline failed to paint has not been read.
export function postIssueRendered(): void {
  postToNative({ type: "issueRendered" });
}

/// Open another card on this board.
export function openIssue(number: number): void {
  postToNative({ type: "openIssue", number });
}

/// Open a run's transcript.
export function openRun(attempt: number): void {
  postToNative({ type: "openRun", attempt });
}

/// The description editor's Done. Native does the PATCH — this page never
/// speaks REST, exactly as the transcript never does.
export function postDescription(text: string): void {
  postToNative({ type: "descriptionDone", text });
}

/// Ask native to open a picker for a field the header/chips own.
export function pickField(field: "status" | "priority" | "assignee" | "stage"): void {
  postToNative({ type: "pick", field });
}

/// How tall the rendered card is, so native can decide whether its "new
/// activity" pill is worth showing.
export function postActivityAtBottom(atBottom: boolean): void {
  postToNative({ type: "activityAtBottom", atBottom });
}
