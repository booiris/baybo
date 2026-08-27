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
  targetId: string;
  /// Bottom chrome (the dock, plus a ridden keyboard) at first paint, in CSS
  /// px. Streamed from then on — the webview never resizes.
  bottomInset: number;
  restoredState?: IssueViewState;
};

export type IssueViewState = {
  scrollTop: number;
  folds: Record<string, boolean>;
};

export type IssuePresentation = {
  init: IssueInit;
  payload: IssuePayload;
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

/// What the RENDERED CARD listens to — and only it.
///
/// `init` and `setLanguage` are deliberately NOT here: they have their own
/// listeners below because this is a one-holder slot, and a second consumer
/// parked in it swallows the card. See `subscribeIssue`.
export type IssueEvents = {
  /// A full replacement. The card is small and its parts move together — a
  /// comment writes a timeline entry AND bumps the card — so there is nothing
  /// a field-by-field merge would protect and one more way for the two to
  /// disagree if it tried.
  deliver(payload: IssuePayload): void;
  bottomInset(px: number): void;
  /// Scroll the newest activity into view (the "new activity" pill, and the
  /// jump native runs after a comment lands).
  jumpToLatest(): void;
};

export type IssueGlobal = {
  init(payload: IssueInit): void;
  deliver(payload: IssuePayload): void;
  setBottomInset(px: number): void;
  setLanguage(lang: string): void;
  jumpToLatest(): void;
  snapshotState(): IssueViewState | null;
};

type Buffered =
  | { kind: "deliver"; payload: IssuePayload }
  | { kind: "bottomInset"; px: number }
  | { kind: "jumpToLatest" };

let events: IssueEvents | null = null;
/// Everything native pushed before the card subscribed, replayed in arrival
/// order the moment it does. Load-bearing on EVERY open, not a cold-start
/// nicety: native answers `issueReady` in that same turn with whatever it
/// already holds — a mirror that was on disk before the webview existed, or a
/// fetch a directly-connected gateway answered while the page was still
/// parsing — and React has not committed the tree yet.
const buffer: Buffered[] = [];

/// The init native sent, LATCHED.
///
/// It arrives with `ready`, before the React tree mounts, and it carries the
/// language and the dock's height — so a listener that registers afterwards
/// must still be told, exactly as the transcript's `onInit` does.
let initPayload: IssueInit | null = null;
const initListeners = new Set<(payload: IssueInit) => void>();
let presentation: IssuePresentation | null = null;
const presentationListeners = new Set<(value: IssuePresentation) => void>();
const languageListeners = new Set<(lang: string) => void>();
let stateProvider: (() => IssueViewState | null) | null = null;
const retargetingClass = "issue-retargeting";

/// Hear the init — now if it has already landed, otherwise when it does.
export function onIssueInit(cb: (payload: IssueInit) => void): () => void {
  initListeners.add(cb);
  if (initPayload !== null) cb(initPayload);
  return () => {
    initListeners.delete(cb);
  };
}

/// Switch React trees only when the new target's first frame is present.
///
/// `init` and `deliver` are two native evals. Rendering from `init` alone
/// creates a real intermediate page with `payload === null`: the reused slot
/// briefly shows its old card, then "Loading card", then the new card. This
/// latch hands the key and its first payload to React in one state update.
export function onIssuePresentation(
  cb: (value: IssuePresentation) => void,
): () => void {
  presentationListeners.add(cb);
  if (presentation !== null) cb(presentation);
  return () => {
    presentationListeners.delete(cb);
  };
}

/// Hear native switch the language. Not latched: `init` already carries the
/// language this page opened in, and this is only the changes after it.
export function onIssueLanguage(cb: (lang: string) => void): () => void {
  languageListeners.add(cb);
  return () => {
    languageListeners.delete(cb);
  };
}

function dispatch(item: Buffered): void {
  if (!events) {
    buffer.push(item);
    return;
  }
  deliver(events, item);
}

function deliver(e: IssueEvents, item: Buffered): void {
  if (item.kind === "deliver") e.deliver(item.payload);
  else if (item.kind === "bottomInset") e.bottomInset(item.px);
  // Every kind needs its own branch ABOVE this terminal else — it is a bare
  // fall-through to `jumpToLatest`, so a missing branch silently turns a new
  // command into "scroll to the bottom", and the type checker cannot see it.
  // `bridge.test.ts` pins the same rule for the transcript.
  else e.jumpToLatest();
}

/// Take the card's ONE subscription, and drain what arrived before it.
///
/// One holder, and it is the React tree. Nothing else may subscribe: whoever
/// holds this slot CONSUMES native's stream, so a second listener parked here
/// — a language shim, a logger — is handed the card's first `deliver` and
/// drops it on the floor, and no second one is ever sent. The card then sits
/// on its loading line with the data already in the app, which is what
/// `deliverBeforeMount.test.tsx` pins.
export function subscribeIssue(e: IssueEvents): () => void {
  events = e;
  for (const item of buffer.splice(0, buffer.length)) deliver(e, item);
  return () => {
    if (events === e) events = null;
  };
}

export function provideIssueState(provider: () => IssueViewState | null): () => void {
  stateProvider = provider;
  return () => {
    if (stateProvider === provider) stateProvider = null;
  };
}

window.issuePage = {
  init: (payload) => {
    if (initPayload !== null && initPayload.targetId !== payload.targetId) {
      events = null;
      buffer.length = 0;
      stateProvider = null;
      presentation = null;
    }
    initPayload = payload;
    document.getElementById("issue-root")?.classList.add(retargetingClass);
    for (const cb of [...initListeners]) cb(payload);
  },
  deliver: (payload) => {
    if (initPayload !== null) {
      presentation = { init: initPayload, payload };
      for (const cb of [...presentationListeners]) cb(presentation);
    }
    dispatch({ kind: "deliver", payload });
  },
  setBottomInset: (px) => dispatch({ kind: "bottomInset", px }),
  setLanguage: (lang) => {
    for (const cb of [...languageListeners]) cb(lang);
  },
  jumpToLatest: () => dispatch({ kind: "jumpToLatest" }),
  snapshotState: () => stateProvider?.() ?? null,
};

/// Reveal only the tree that still belongs to the bridge's current target.
/// A late layout effect from the outgoing keyed tree must not expose it.
export function revealIssueTarget(targetId: string): void {
  if (initPayload?.targetId !== targetId) return;
  document.getElementById("issue-root")?.classList.remove(retargetingClass);
}

// ---- outbound --------------------------------------------------------------

export function postIssueReady(): void {
  postToNative({ type: "issueReady" });
}

/// The card rendered. Native stamps it read only now — never on the way in,
/// because a card whose timeline failed to paint has not been read.
export function postIssueRendered(targetId: string = activeTargetId()): void {
  postToNative({ type: "issueRendered", targetId });
}

/// Open another card on this board.
export function openIssue(number: number): void {
  postToNative({ type: "openIssue", targetId: activeTargetId(), number });
}

/// Open a run's transcript.
export function openRun(attempt: number): void {
  postToNative({ type: "openRun", targetId: activeTargetId(), attempt });
}

/// Ask native to open a picker for a field the header/chips own.
export function pickField(field: "status" | "priority" | "assignee" | "stage"): void {
  postToNative({ type: "pick", targetId: activeTargetId(), field });
}

/// A face this page drew for an agent that has none, as PNG base64.
///
/// Native does the storing: uploading needs the blob API and the agent PUT,
/// and this page speaks no REST — exactly as it never sends a comment itself.
/// Fire-and-forget: the answer arrives as the next delivery, with the agent's
/// `avatar` filled in.
export function postGeneratedFace(targetId: string, agentId: string, pngBase64: string): void {
  postToNative({ type: "generatedFace", targetId, agentId, pngBase64 });
}

/// How tall the rendered card is, so native can decide whether its "new
/// activity" pill is worth showing.
export function postActivityAtBottom(targetId: string, atBottom: boolean): void {
  postToNative({ type: "activityAtBottom", targetId, atBottom });
}

export function postIssueState(targetId: string, state: IssueViewState): void {
  postToNative({ type: "issueState", targetId, ...state });
}

function activeTargetId(): string {
  return initPayload?.targetId ?? "test";
}
