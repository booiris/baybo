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

type BlobResultPayload = {
  id: number;
  dataBase64: string | null;
  mimeType: string;
  error: string | null;
};

/// A file attachment's lifecycle, owned by native (the blob cache is on-device
/// and iOS may purge it, so `ready` is a fact about disk, never a memory).
export type FileState = "idle" | "loading" | "ready" | "failed";

export type FileStatePayload = {
  blobId: string;
  state: FileState;
  /// Bytes on disk while `loading`. `total` is the blob's full length when the
  /// server declared one — the card already knows it from the attachment.
  loaded?: number;
  total?: number;
  error?: string;
};

type BayboGlobal = {
  init(payload: InitPayload): void;
  pushFrame(frameJson: string): void;
  setConnEpoch(epoch: number): void;
  userSent(payload: UserSentPayload): void;
  sendFailed(msgId: string): void;
  blobResult(payload: BlobResultPayload): void;
  fileState(payload: FileStatePayload): void;
  setLanguage(lang: string): void;
  setBottomInset(px: number): void;
  jumpToLatest(): void;
  /// Native asks the transcript to run the sync loop with its current cursor
  /// (offscreen buffer overflow re-attach; any native-side "go sync" edge).
  /// The web side answers by posting `{type:"sync"}` back with the cursor.
  requestSync(): void;
  /// Native invokes this just before it detaches the bridge (back-out) so the
  /// debounced transcript mirror is written up to that instant — otherwise
  /// steps delivered live since the last debounce sit in neither the mirror nor
  /// the native frame buffer and vanish from the work block on re-entry.
  flushPersist(): void;
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

/// Run the one forward-recovery pull: native fetches
/// `GET /v1/chat/sessions/{id}/sync?since_ordinal=…&limit=…` over the active
/// leg and pushes the result back as a local `sync_page` frame (or
/// `sync_failed` on error). `sinceOrdinal` is the transcript's cursor —
/// `null` means "baseline me on the newest page".
export function postSyncRequest(sinceOrdinal: number | null, limit: number): void {
  post({ type: "sync", sinceOrdinal, limit });
}

/// Advance the server chat-list read cursor to `ordinal` — the viewer (looking
/// at this transcript) has read up to here. Native forwards it to
/// `chat_mark_read`; the unread badge clears on the next list pull. Best-effort
/// (max-wins server-side), so a stale/duplicate marker is harmless.
export function postMarkRead(ordinal: number): void {
  postSafe({ type: "mark_read", ordinal });
}

// The jump-to-latest button is native (liquid glass, above the composer) —
// the transcript mirrors its visibility state over; taps come back through
// the `jumpToLatest` transcript event.
export function postJumpVisible(visible: boolean): void {
  postSafe({ type: "jumpVisible", visible });
}

/// The composer's send button is native and flips to a stop affordance while a
/// turn is in flight. The webview is the single source of truth for turn state
/// (SubscribeState reconstruction, the finalization-race handling), so it mirrors
/// the run state over; a `/stop` tap comes back as an ordinary chat send native
/// side. Fire-and-forget; native dedups.
export function postRunState(running: boolean): void {
  postSafe({ type: "runState", running });
}

/// Markdown links must not navigate the transcript webview away — native opens
/// them in the system browser instead. Dev browser: plain window.open.
export function openUrl(url: string): void {
  if (native) postSafe({ type: "openUrl", url });
  else window.open(url, "_blank", "noopener");
}

/// Copy text to the system clipboard. Native owns the write (UIPasteboard) and
/// fires the confirming haptic — a WKWebView rejects `navigator.clipboard`
/// writes outside a live user gesture, and a long-press timer has none. Dev
/// browser: best-effort clipboard write so the affordance still works there.
export function copyText(text: string): void {
  if (native) postSafe({ type: "copy", text });
  else void navigator.clipboard?.writeText(text).catch(() => {});
}

/// Open a tapped image full-screen. Native decodes the device-cached blob and
/// presents its own zoomable viewer (pinch, double-tap-to-restore, black field)
/// — images take this path; non-image files take `previewFile` (QuickLook). Name
/// + mime ride along so the viewer's share sheet can hand over the file under its
/// real name (an agent image usually carries no name; the mime derives one).
export function viewImage(blobId: string, filename: string, mimeType: string): void {
  postSafe({ type: "viewImage", blobId, filename, mimeType });
}

// History fetches are fire-and-forget from Web's perspective: native calls the
// chat API and pushes back a local `history_page` / `history_failed` frame.
export function fetchHistory(beforeOrdinal: number | null, limit: number): void {
  post({ type: "fetchHistory", beforeOrdinal, limit });
}

// Retry a failed send: re-post the full payload (native reuses the msgId as the
// idempotency key, so no duplicate lands). Web carries the payload — not native —
// so the retry dot still works after an eviction/relaunch rebuilt the bubble.
export function retrySend(payload: UserSentPayload): void {
  post({ type: "retry", msgId: payload.msgId, text: payload.text, attachments: payload.attachments });
}

// ---- persistence -----------------------------------------------------------

const PERSIST_DEBOUNCE_MS = 500;

let persistTimer: ReturnType<typeof setTimeout> | undefined;
// A delayed flush must write to the session that produced it, not the current one.
let pendingPersist: { sessionId: string; state: PersistedState } | null = null;

function flushPersist(): void {
  clearTimeout(persistTimer);
  persistTimer = undefined;
  if (pendingPersist === null) return;
  const { sessionId, state } = pendingPersist;
  pendingPersist = null;
  postSafe({ type: "persist", sessionId, state });
}

/// Replaces the old localStorage saveChatState: debounced so a catch-up burst
/// collapses into one write; native owns the durable copy. localStorage is not
/// used at all (file:// storage is unreliable, and native restores via init).
export function persistState(state: PersistedState): void {
  const sessionId = initPayload?.sessionId;
  if (sessionId === undefined) return;
  pendingPersist = { sessionId, state };
  clearTimeout(persistTimer);
  persistTimer = setTimeout(flushPersist, PERSIST_DEBOUNCE_MS);
}

// A backgrounded WKWebView can be torn down before the debounce fires.
window.addEventListener("pagehide", flushPersist);

// ---- blob fetch ------------------------------------------------------------

let blobReqId = 0;
const blobPending = new Map<
  number,
  { resolve: (url: string) => void; reject: (err: Error) => void; fallbackMime: string }
>();

/// Fetch attachment bytes via native (device-cached blob leg) and wrap them in
/// an object URL for an <img> src. The caller owns the URL and must
/// URL.revokeObjectURL it when done.
export function blobObjectUrl(blobId: string, mimeType: string): Promise<string> {
  if (!native) return Promise.reject(new Error("no native bridge"));
  blobReqId += 1;
  const id = blobReqId;
  return new Promise((resolve, reject) => {
    blobPending.set(id, { resolve, reject, fallbackMime: mimeType });
    try {
      post({ type: "requestBlob", id, blobId });
    } catch (e) {
      blobPending.delete(id);
      reject(new Error(String(e)));
    }
  });
}

function settleBlob(payload: BlobResultPayload): void {
  const pending = blobPending.get(payload.id);
  if (!pending) return;
  blobPending.delete(payload.id);
  if (payload.dataBase64 === null) {
    pending.reject(new Error(payload.error ?? "blob fetch failed"));
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

// ---- file attachments (download / preview) ---------------------------------

/// Keyed by blob id rather than fanned through the transcript's reducer: every
/// card subscribes for itself, so a progress tick re-renders one card instead of
/// the whole thread (and `MessageRow`'s memo survives).
const fileStateListeners = new Map<string, Set<(payload: FileStatePayload) => void>>();

export function onFileState(
  blobId: string,
  listener: (payload: FileStatePayload) => void,
): () => void {
  let listeners = fileStateListeners.get(blobId);
  if (!listeners) {
    listeners = new Set();
    fileStateListeners.set(blobId, listeners);
  }
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0) fileStateListeners.delete(blobId);
  };
}

/// Ask native whether the blob is already on disk. Answered with a `fileState`.
export function queryFileState(blobId: string): void {
  postSafe({ type: "queryFileState", blobId });
}

/// Start (or join) the download. Native streams `loading` ticks, then `ready`.
export function downloadFile(blobId: string): void {
  postSafe({ type: "downloadFile", blobId });
}

/// Open the downloaded file — QuickLook where iOS can render it, the share
/// sheet otherwise. Native needs the name and mime to pick the previewer.
export function previewFile(blobId: string, filename: string, mimeType: string): void {
  postSafe({ type: "previewFile", blobId, filename, mimeType });
}

// ---- inbound dispatch ------------------------------------------------------

export type TranscriptEvents = {
  frame(frameJson: string): void;
  connEpoch(epoch: number): void;
  userSent(payload: UserSentPayload): void;
  /// Native's send Task errored — mark that optimistic bubble failed (red
  /// retry dot). Keyed by the msgId native minted in `userSent`.
  sendFailed(msgId: string): void;
  /// Native chrome covering the webview's bottom edge (composer + ridden
  /// keyboard), in CSS px. Streams per layout tick through keyboard
  /// animations.
  bottomInset(px: number): void;
  /// The native jump-to-latest button was tapped — run the glide.
  jumpToLatest(): void;
  /// Native asked for a sync run (buffer-overflow re-attach etc.).
  syncRequested(): void;
};

type Buffered =
  | { kind: "frame"; frameJson: string }
  | { kind: "epoch"; epoch: number }
  | { kind: "userSent"; payload: UserSentPayload }
  | { kind: "sendFailed"; msgId: string }
  | { kind: "bottomInset"; px: number }
  | { kind: "jumpToLatest" }
  | { kind: "syncRequested" };

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
  else if (item.kind === "sendFailed") e.sendFailed(item.msgId);
  else if (item.kind === "bottomInset") e.bottomInset(item.px);
  else if (item.kind === "syncRequested") e.syncRequested();
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
    // Native flushes the old mirror before retargeting; do not post stale state here.
    buffer.length = 0;
    blobPending.clear();
    clearTimeout(persistTimer);
    persistTimer = undefined;
    pendingPersist = null;
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
  sendFailed(msgId) {
    dispatch({ kind: "sendFailed", msgId });
  },
  blobResult(payload) {
    settleBlob(payload);
  },
  fileState(payload) {
    // A tick for a blob no card is showing (the session switched mid-download)
    // simply has no listener.
    for (const listener of fileStateListeners.get(payload.blobId) ?? []) listener(payload);
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
  requestSync() {
    dispatch({ kind: "syncRequested" });
  },
  flushPersist() {
    flushPersist();
  },
};
