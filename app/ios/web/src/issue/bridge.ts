import { postToNative } from "../bridge";
import type { ChildIssue, IssueDetail, IssueEvent, IssueRun } from "./types";


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

export type Person = { handle: string; avatar?: string; monogram: string };

export type IssuePayload = {
  issue: IssueDetail;
  events: IssueEvent[];
  /// True only after this visit fetched the timeline from the gateway. Mirror
  /// content may paint, but it must never advance the server read cursor.
  timelineLive?: boolean;
  pendingComments?: IssueEvent[];
  runs: IssueRun[];
  /// Agent profile id → who they are. An id that resolves to nothing prints as
  /// itself, which is what the gateway does too.
  people: Record<string, Person>;
  /// This card's children, from the board. The issue DTO carries only a
  /// done/total COUNT, so listing them needs a source the card itself is not.
  children?: ChildIssue[];
  /// Server-resolved live boundary. Absence means "do not change the boundary",
  /// not "clear it", because the page latches the first one it receives.
  firstUnread?: string;
};

export type IssueEvents = {
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
// Native can deliver in the same turn that React schedules its mount. Buffering
// here is what makes that pre-subscription payload observable.
const buffer: Buffered[] = [];

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
  else e.jumpToLatest();
}

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

export function postGeneratedFace(targetId: string, agentId: string, pngBase64: string): void {
  postToNative({ type: "generatedFace", targetId, agentId, pngBase64 });
}

/// How tall the rendered card is, so native can decide whether its "new
/// activity" pill is worth showing.
export function postActivityAtBottom(targetId: string, atBottom: boolean): void {
  postToNative({ type: "activityAtBottom", targetId, atBottom });
}

export function retryComment(clientMsgId: string): void {
  postToNative({ type: "retryComment", targetId: activeTargetId(), clientMsgId });
}

export function postIssueState(targetId: string, state: IssueViewState): void {
  postToNative({ type: "issueState", targetId, ...state });
}

function activeTargetId(): string {
  return initPayload?.targetId ?? "test";
}
