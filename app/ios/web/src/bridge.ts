// The native<->web seam. Native drives the page through `window.baybo`
// (installed synchronously below, before anything else runs); the page talks
// back through `window.webkit.messageHandlers.baybo.postMessage`. In a plain
// dev browser (no window.webkit) outbound posts degrade to console.log stubs.

import type { PersistedState, WireAttachment } from "./types";

export type InitPayload = {
  language: string;
  sessionId: string;
  restoredState: PersistedState | null;
  connEpoch: number;
};

export type UserSentPayload = {
  msgId: string;
  text: string;
  attachments: WireAttachment[];
};

type ImageResultPayload = {
  id: number;
  dataBase64: string | null;
  mimeType: string;
  error: string | null;
};

type BayboGlobal = {
  init(payload: InitPayload): void;
  pushFrame(frameJson: string): void;
  setConnEpoch(epoch: number): void;
  userSent(payload: UserSentPayload): void;
  imageResult(payload: ImageResultPayload): void;
  setLanguage(lang: string): void;
  setBottomInset(px: number): void;
  jumpToLatest(): void;
};

declare global {
  interface Window {
    baybo: BayboGlobal;
    webkit?: {
      messageHandlers?: {
        baybo?: { postMessage(message: unknown): void };
      };
    };
  }
}

const native = window.webkit?.messageHandlers?.baybo;

export const hasNativeBridge = native !== undefined;

function post(message: Record<string, unknown>): void {
  if (native) native.postMessage(message);
  else console.log("[baybo bridge]", message);
}

// For fire-and-forget posts whose failure must never surface as a page error
// (persist/ordinal/log) — a throwing log post would recurse through onerror.
function postSafe(message: Record<string, unknown>): void {
  try {
    post(message);
  } catch {
    /* bridge unavailable — best-effort */
  }
}

export function log(level: "info" | "warn" | "error", message: string): void {
  postSafe({ type: "log", level, message });
}

window.addEventListener("error", (e) => {
  log("error", `${e.message} (${e.filename || "?"}:${e.lineno})`);
});
window.addEventListener("unhandledrejection", (e) => {
  log("error", `unhandled rejection: ${String(e.reason)}`);
});

export function postReady(): void {
  post({ type: "ready" });
}

/// The transcript has rendered its first frame — lets native fade the webview
/// in rather than popping the content in when the chat screen slides on.
export function postContentReady(): void {
  postSafe({ type: "shown" });
}

export function postOrdinal(lastOrdinal: number | null): void {
  postSafe({ type: "ordinal", lastOrdinal });
}

// The jump-to-latest button is native (liquid glass, above the composer) —
// the transcript mirrors its visibility state over; taps come back through
// the `jumpToLatest` transcript event.
export function postJumpVisible(visible: boolean): void {
  postSafe({ type: "jumpVisible", visible });
}

/// Markdown links must not navigate the transcript webview away — native opens
/// them in the system browser instead. Dev browser: plain window.open.
export function openUrl(url: string): void {
  if (native) postSafe({ type: "openUrl", url });
  else window.open(url, "_blank", "noopener");
}

// Fetches are fire-and-forget too, but throwing is useful to the caller: the
// transcript's recover/paging paths surface a failed post as a notice bubble
// (the old invoke() rejection path).
export function fetchHistory(beforeOrdinal: number | null, limit: number): void {
  post({ type: "fetchHistory", beforeOrdinal, limit });
}

// ---- persistence -----------------------------------------------------------

const PERSIST_DEBOUNCE_MS = 500;

let persistTimer: ReturnType<typeof setTimeout> | undefined;
let pendingPersist: PersistedState | null = null;

function flushPersist(): void {
  clearTimeout(persistTimer);
  persistTimer = undefined;
  if (pendingPersist === null) return;
  const state = pendingPersist;
  pendingPersist = null;
  postSafe({ type: "persist", state });
}

/// Replaces the old localStorage saveChatState: debounced so a catch-up burst
/// collapses into one write; native owns the durable copy. localStorage is not
/// used at all (file:// storage is unreliable, and native restores via init).
export function persistState(state: PersistedState): void {
  pendingPersist = state;
  clearTimeout(persistTimer);
  persistTimer = setTimeout(flushPersist, PERSIST_DEBOUNCE_MS);
}

// A backgrounded WKWebView can be torn down before the debounce fires.
window.addEventListener("pagehide", flushPersist);

// ---- image fetch (replaces invoke("blob_image")) ---------------------------

let imageReqId = 0;
const imagePending = new Map<
  number,
  { resolve: (url: string) => void; reject: (err: Error) => void; fallbackMime: string }
>();

/// Fetch attachment bytes via native (device-cached blob leg) and wrap them in
/// an object URL for an <img> src. The caller owns the URL and must
/// URL.revokeObjectURL it when done.
export function imageObjectUrl(blobId: string, mimeType: string): Promise<string> {
  if (!native) return Promise.reject(new Error("no native bridge"));
  imageReqId += 1;
  const id = imageReqId;
  return new Promise((resolve, reject) => {
    imagePending.set(id, { resolve, reject, fallbackMime: mimeType });
    try {
      post({ type: "requestImage", id, blobId });
    } catch (e) {
      imagePending.delete(id);
      reject(new Error(String(e)));
    }
  });
}

function settleImage(payload: ImageResultPayload): void {
  const pending = imagePending.get(payload.id);
  if (!pending) return;
  imagePending.delete(payload.id);
  if (payload.dataBase64 === null) {
    pending.reject(new Error(payload.error ?? "image fetch failed"));
    return;
  }
  try {
    const raw = atob(payload.dataBase64);
    const bytes = new Uint8Array(raw.length);
    for (let i = 0; i < raw.length; i += 1) bytes[i] = raw.charCodeAt(i);
    const mime = payload.mimeType || pending.fallbackMime || "application/octet-stream";
    pending.resolve(URL.createObjectURL(new Blob([bytes], { type: mime })));
  } catch (e) {
    pending.reject(new Error(String(e)));
  }
}

// ---- inbound dispatch ------------------------------------------------------

export type TranscriptEvents = {
  frame(frameJson: string): void;
  connEpoch(epoch: number): void;
  userSent(payload: UserSentPayload): void;
  /// Native chrome covering the webview's bottom edge (composer + ridden
  /// keyboard), in CSS px. Streams per layout tick through keyboard
  /// animations.
  bottomInset(px: number): void;
  /// The native jump-to-latest button was tapped — run the glide.
  jumpToLatest(): void;
};

type Buffered =
  | { kind: "frame"; frameJson: string }
  | { kind: "epoch"; epoch: number }
  | { kind: "userSent"; payload: UserSentPayload }
  | { kind: "bottomInset"; px: number }
  | { kind: "jumpToLatest" };

let initPayload: InitPayload | null = null;
let onInitCb: ((payload: InitPayload) => void) | null = null;
let onLanguageCb: ((lang: string) => void) | null = null;
let events: TranscriptEvents | null = null;
// Anything native pushes before the transcript subscribes (it renders only
// after init lands) is replayed in arrival order on subscription.
const buffer: Buffered[] = [];
let connEpoch = 0;

export function currentConnEpoch(): number {
  return connEpoch;
}

export function onInit(cb: (payload: InitPayload) => void): void {
  onInitCb = cb;
  if (initPayload) cb(initPayload);
}

export function onLanguage(cb: (lang: string) => void): void {
  onLanguageCb = cb;
}

export function subscribeTranscript(e: TranscriptEvents): () => void {
  events = e;
  const queued = buffer.splice(0, buffer.length);
  for (const item of queued) {
    deliver(e, item);
  }
  return () => {
    if (events === e) events = null;
  };
}

function deliver(e: TranscriptEvents, item: Buffered): void {
  if (item.kind === "frame") e.frame(item.frameJson);
  else if (item.kind === "epoch") e.connEpoch(item.epoch);
  else if (item.kind === "userSent") e.userSent(item.payload);
  else if (item.kind === "bottomInset") e.bottomInset(item.px);
  else e.jumpToLatest();
}

function dispatch(item: Buffered): void {
  if (!events) {
    buffer.push(item);
    return;
  }
  deliver(events, item);
}

window.baybo = {
  init(payload) {
    connEpoch = payload.connEpoch;
    initPayload = payload;
    onInitCb?.(payload);
  },
  pushFrame(frameJson) {
    dispatch({ kind: "frame", frameJson });
  },
  setConnEpoch(epoch) {
    connEpoch = epoch;
    dispatch({ kind: "epoch", epoch });
  },
  userSent(payload) {
    dispatch({ kind: "userSent", payload });
  },
  imageResult(payload) {
    settleImage(payload);
  },
  setLanguage(lang) {
    onLanguageCb?.(lang);
  },
  setBottomInset(px) {
    dispatch({ kind: "bottomInset", px });
  },
  jumpToLatest() {
    dispatch({ kind: "jumpToLatest" });
  },
};
