// The native<->web seam. Native drives the page through `window.baybo`
// (installed synchronously below, before anything else runs); the page talks
// back through `window.webkit.messageHandlers.baybo.postMessage`. In a plain
// dev browser (no window.webkit) outbound posts degrade to console.log stubs.

import type { OutlineEntry, PersistedState, WireAttachment } from "./types";
import {
  HTML_PREVIEW_COLLAPSE_EVENT,
  HTML_PREVIEW_DRAG_BEGIN_EVENT,
  HTML_PREVIEW_DRAG_END_EVENT,
  HTML_PREVIEW_DRAG_MOVE_EVENT,
} from "./htmlPreviewProtocol";

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

/// Where the native audio engine is for one blob. `stopped` covers everything
/// that isn't the current track — never loaded, another track took the player,
/// or playback ran to the end (position rewound to 0).
export type AudioPlayState = "playing" | "paused" | "stopped";

export type AudioStatePayload = {
  blobId: string;
  state: AudioPlayState;
  /// Seconds. `duration` is 0 until the asset's metadata has loaded.
  position: number;
  duration: number;
};

type VideoPosterResultPayload = {
  id: number;
  dataBase64: string | null;
  /// Natural pixel size of the poster frame (post-rotation).
  width: number;
  height: number;
  durationMs: number;
  error: string | null;
};

export type VideoPoster = {
  url: string;
  width: number;
  height: number;
  durationMs: number;
};

type BayboGlobal = {
  init(payload: InitPayload): void;
  pushFrame(frameJson: string): void;
  setConnEpoch(epoch: number): void;
  userSent(payload: UserSentPayload): void;
  sendFailed(msgId: string): void;
  sendConfirmed(msgId: string): void;
  blobResult(payload: BlobResultPayload): void;
  fileState(payload: FileStatePayload): void;
  audioState(payload: AudioStatePayload): void;
  videoPoster(payload: VideoPosterResultPayload): void;
  setLanguage(lang: string): void;
  setBottomInset(px: number): void;
  jumpToLatest(): void;
  /// A row of the native message-index sheet was tapped — park that user
  /// message under the header veil. `rowId` is an `OutlineEntry.id`.
  jumpToMessage(rowId: string): void;
  /// A native search result was tapped. Addressed BY ORDINAL, never by row id:
  /// a user row is keyed by its `platform_msg_id` with the ordinal carried
  /// beside it, so building `m<ordinal>` would resolve only agent rows and
  /// silently miss every user-authored hit. The thread pages backward on its own
  /// if the row is not loaded yet (see `JUMP_PAGE_BUDGET`).
  jumpToOrdinal(ordinal: number): void;
  /// The sheet's "load earlier" row — runs the transcript's own backward
  /// paging, which grows the outline when the prepend lands.
  outlineLoadOlder(): void;
  /// The sheet is opening: scan for the user message currently at the top of
  /// the viewport and answer with `{type:"outlineHere"}`. Pulled rather than
  /// pushed — a live scan would force a layout per scroll tick.
  requestOutlineHere(): void;
  /// Native asks the transcript to run the sync loop with its current cursor
  /// (offscreen buffer overflow re-attach; any native-side "go sync" edge).
  /// The web side answers by posting `{type:"sync"}` back with the cursor.
  requestSync(): void;
  /// Collapse an inline HTML preview before the transcript is detached.
  collapseHtmlPreview(): void;
  /// A left-edge swipe over a FULL-SCREEN preview. Native holds the interactive
  /// pop off while one is up (PopGesture.swift) and streams the drag here
  /// instead, so the swipe leaves the preview rather than the conversation.
  /// `px` is the distance travelled from the edge; `dismiss` is native's
  /// verdict on the release (distance or flick), never re-judged here.
  htmlPreviewDragBegin(): void;
  htmlPreviewDragMove(px: number): void;
  htmlPreviewDragEnd(dismiss: boolean): void;
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
        /// The deck shell's handler (src/deck/bridge.ts) — declared here
        /// because a global interface may only be declared once per shape.
        deck?: { postMessage(message: unknown): void };
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

/// The message-index sheet is native, but only the web tree knows which of the
/// user's sends are in it (the optimistic bubble exists here before any echo,
/// `/stop` echoes are filtered here, and the loaded window is this tree's).
/// So the transcript owns the list and mirrors it over.
export type OutlinePost = {
  entries: OutlineEntry[];
  /// Older pages remain on the server — the sheet says `24+`, not `24`.
  hasMoreOlder: boolean;
  loadingOlder: boolean;
  /// Whether the affordance is worth showing at all. The web decides, because
  /// it is the only layer that can count.
  available: boolean;
};

const OUTLINE_DEBOUNCE_MS = 250;

let outlineTimer: ReturnType<typeof setTimeout> | undefined;
// The last payload actually posted, as JSON. Exact identity rather than a
// cheap signature: a `sendState` flip to undefined, or `createdAt` adopting the
// server clock, changes nothing a length/last-id signature would catch. We must
// stringify to post anyway, so the comparison is free.
let outlineLastJson = "";
let outlinePending: OutlinePost | null = null;
// The first post after init goes out immediately, so the header button is live
// as the screen slides on; every later one is trailing-debounced.
let outlinePosted = false;

function flushOutline(): void {
  clearTimeout(outlineTimer);
  outlineTimer = undefined;
  if (outlinePending === null) return;
  const state = outlinePending;
  outlinePending = null;
  outlinePosted = true;
  postSafe({ type: "outline", ...state });
}

export function postOutline(state: OutlinePost): void {
  const json = JSON.stringify(state);
  if (json === outlineLastJson) return;
  outlineLastJson = json;
  outlinePending = state;
  if (!outlinePosted) {
    flushOutline();
    return;
  }
  clearTimeout(outlineTimer);
  outlineTimer = setTimeout(flushOutline, OUTLINE_DEBOUNCE_MS);
}

/// Re-post even when the payload is byte-identical — the self-heal path, taken
/// when native asks to jump to a row this tree no longer holds. The identity
/// guard exists to suppress redundant posts, and here the redundant post IS the
/// point: native is holding a list we have to overwrite.
export function resendOutline(state: OutlinePost): void {
  outlineLastJson = JSON.stringify(state);
  outlinePending = state;
  flushOutline();
}

/// Answer to `requestOutlineHere`: which listed message the reader is parked
/// on, so the sheet opens scrolled to it. `null` when nothing is above the fold.
export function postOutlineHere(rowId: string | null): void {
  postSafe({ type: "outlineHere", rowId });
}

/// The composer's send button is native and flips to a stop affordance while a
/// turn is in flight. The webview is the single source of truth for turn state
/// (SubscribeState reconstruction, the finalization-race handling), so it mirrors
/// the run state over; a `/stop` tap comes back as an ordinary chat send native
/// side. Fire-and-forget; native dedups.
export function postRunState(running: boolean): void {
  postSafe({ type: "runState", running });
}

/// An iframe expanded over the transcript needs native chrome out of its way.
/// Only the trusted main frame can reach the native handler; the sandboxed
/// preview subframe is rejected by TranscriptBridge's main-frame gate.
export function postHtmlPreviewMaximized(maximized: boolean): void {
  postSafe({ type: "htmlPreviewMaximized", maximized });
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

/// Hand the downloaded blob to the system share sheet under its real name (a
/// long-press on a file / audio / video card — the image viewer has its own
/// share button). Native materialises the file exactly like previewFile.
export function shareFile(blobId: string, filename: string, mimeType: string): void {
  postSafe({ type: "shareFile", blobId, filename, mimeType });
}

// ---- audio playback (native engine) -----------------------------------------

/// Same per-blob fan-out as `fileStateListeners`: a 2 Hz position tick
/// re-renders one audio card, not the thread.
const audioStateListeners = new Map<string, Set<(payload: AudioStatePayload) => void>>();

export function onAudioState(
  blobId: string,
  listener: (payload: AudioStatePayload) => void,
): () => void {
  let listeners = audioStateListeners.get(blobId);
  if (!listeners) {
    listeners = new Set();
    audioStateListeners.set(blobId, listeners);
  }
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0) audioStateListeners.delete(blobId);
  };
}

/// Ask native where its (single, app-wide) audio player is for this blob —
/// answered with an `audioState` push. A card mounting mid-playback (session
/// switch, thread reload) resyncs itself this way.
export function queryAudioState(blobId: string): void {
  postSafe({ type: "queryAudioState", blobId });
}

/// Play/pause. The engine is native (AVPlayer on the device-cached blob):
/// bytes never cross the bridge, the ringer switch can't silence it, and
/// playback survives leaving the chat. Starting one track stops any other.
export function audioToggle(blobId: string, filename: string, mimeType: string): void {
  postSafe({ type: "audioToggle", blobId, filename, mimeType });
}

export function audioSeek(blobId: string, position: number): void {
  postSafe({ type: "audioSeek", blobId, position });
}

// ---- video attachments -------------------------------------------------------

/// Open the downloaded video in the native full-screen player (AVKit controls,
/// black field). Name + mime pick the on-disk materialisation, as previewFile.
export function playVideo(blobId: string, filename: string, mimeType: string): void {
  postSafe({ type: "playVideo", blobId, filename, mimeType });
}

let posterReqId = 0;
const posterPending = new Map<
  number,
  { resolve: (poster: VideoPoster) => void; reject: (err: Error) => void }
>();

/// Ask native for a downloaded video's poster frame (AVAssetImageGenerator on
/// the cached file) plus its natural size and duration. The caller owns the
/// object URL and must revoke it.
export function requestVideoPoster(
  blobId: string,
  filename: string,
  mimeType: string,
): Promise<VideoPoster> {
  if (!native) return Promise.reject(new Error("no native bridge"));
  posterReqId += 1;
  const id = posterReqId;
  return new Promise((resolve, reject) => {
    posterPending.set(id, { resolve, reject });
    try {
      post({ type: "requestVideoPoster", id, blobId, filename, mimeType });
    } catch (e) {
      posterPending.delete(id);
      reject(new Error(String(e)));
    }
  });
}

function settleVideoPoster(payload: VideoPosterResultPayload): void {
  const pending = posterPending.get(payload.id);
  if (!pending) return;
  posterPending.delete(payload.id);
  if (payload.dataBase64 === null) {
    pending.reject(new Error(payload.error ?? "poster failed"));
    return;
  }
  try {
    const raw = atob(payload.dataBase64);
    const bytes = new Uint8Array(raw.length);
    for (let i = 0; i < raw.length; i += 1) bytes[i] = raw.charCodeAt(i);
    pending.resolve({
      url: URL.createObjectURL(new Blob([bytes], { type: "image/jpeg" })),
      width: payload.width,
      height: payload.height,
      durationMs: payload.durationMs,
    });
  } catch (e) {
    pending.reject(new Error(String(e)));
  }
}

// ---- inbound dispatch ------------------------------------------------------

export type TranscriptEvents = {
  frame(frameJson: string): void;
  connEpoch(epoch: number): void;
  userSent(payload: UserSentPayload): void;
  /// Native's send Task errored — mark that optimistic bubble failed (red
  /// retry dot). Keyed by the msgId native minted in `userSent`.
  sendFailed(msgId: string): void;
  /// The outbox released that send: the gateway has provably written it, so the
  /// transcript may stop overlaying its optimistic bubble across a REPLACE. The
  /// return leg of `userSent` — nothing on this side can infer it.
  sendConfirmed(msgId: string): void;
  /// Native chrome covering the webview's bottom edge (composer + ridden
  /// keyboard), in CSS px. Streams per layout tick through keyboard
  /// animations.
  bottomInset(px: number): void;
  /// The native jump-to-latest button was tapped — run the glide.
  jumpToLatest(): void;
  /// Native asked for a sync run (buffer-overflow re-attach etc.).
  syncRequested(): void;
  /// A message-index row was tapped — park that user message under the veil.
  jumpToMessage(rowId: string): void;
  /// A search hit was tapped — park the row at `ordinal`, paging backward for
  /// it first if the thread has not loaded that far yet.
  jumpToOrdinal(ordinal: number): void;
  /// The index sheet's "load earlier" row — page the thread backwards.
  outlineLoadOlder(): void;
  /// The index sheet is opening — answer with the reader's current position.
  outlineHereRequested(): void;
};

type Buffered =
  | { kind: "frame"; frameJson: string }
  | { kind: "epoch"; epoch: number }
  | { kind: "userSent"; payload: UserSentPayload }
  | { kind: "sendFailed"; msgId: string }
  | { kind: "sendConfirmed"; msgId: string }
  | { kind: "bottomInset"; px: number }
  | { kind: "jumpToLatest" }
  | { kind: "syncRequested" }
  | { kind: "jumpToMessage"; rowId: string }
  | { kind: "jumpToOrdinal"; ordinal: number }
  | { kind: "outlineLoadOlder" }
  | { kind: "outlineHereRequested" };

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
  else if (item.kind === "sendConfirmed") e.sendConfirmed(item.msgId);
  else if (item.kind === "bottomInset") e.bottomInset(item.px);
  else if (item.kind === "syncRequested") e.syncRequested();
  // Every kind needs its own branch ABOVE the terminal else: that else is a
  // bare fall-through to `jumpToLatest`, so a missing branch silently turns the
  // new command into "scroll to the bottom" — and the type checker cannot see
  // it. `bridge.test.ts` pins this.
  else if (item.kind === "jumpToMessage") e.jumpToMessage(item.rowId);
  else if (item.kind === "jumpToOrdinal") e.jumpToOrdinal(item.ordinal);
  else if (item.kind === "outlineLoadOlder") e.outlineLoadOlder();
  else if (item.kind === "outlineHereRequested") e.outlineHereRequested();
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
    posterPending.clear();
    clearTimeout(persistTimer);
    persistTimer = undefined;
    pendingPersist = null;
    // Per-session caches, same reason as the buffer above: the new tree's first
    // outline must post even if it happens to be byte-identical to the outgoing
    // session's, and it must post IMMEDIATELY so the header button is right as
    // the screen slides on.
    clearTimeout(outlineTimer);
    outlineTimer = undefined;
    outlinePending = null;
    outlineLastJson = "";
    outlinePosted = false;
    connEpoch = payload.connEpoch;
    initPayload = payload;
    window.dispatchEvent(new Event(HTML_PREVIEW_COLLAPSE_EVENT));
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
  sendConfirmed(msgId) {
    dispatch({ kind: "sendConfirmed", msgId });
  },
  blobResult(payload) {
    settleBlob(payload);
  },
  fileState(payload) {
    // A tick for a blob no card is showing (the session switched mid-download)
    // simply has no listener.
    for (const listener of fileStateListeners.get(payload.blobId) ?? []) listener(payload);
  },
  audioState(payload) {
    // Playback outlives the chat screen, so a tick for a track whose card left
    // the tree (session switch) simply has no listener.
    for (const listener of audioStateListeners.get(payload.blobId) ?? []) listener(payload);
  },
  videoPoster(payload) {
    settleVideoPoster(payload);
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
  jumpToMessage(rowId) {
    dispatch({ kind: "jumpToMessage", rowId });
  },
  jumpToOrdinal(ordinal) {
    dispatch({ kind: "jumpToOrdinal", ordinal });
  },
  outlineLoadOlder() {
    dispatch({ kind: "outlineLoadOlder" });
  },
  requestOutlineHere() {
    dispatch({ kind: "outlineHereRequested" });
  },
  requestSync() {
    dispatch({ kind: "syncRequested" });
  },
  collapseHtmlPreview() {
    window.dispatchEvent(new Event(HTML_PREVIEW_COLLAPSE_EVENT));
  },
  htmlPreviewDragBegin() {
    window.dispatchEvent(new Event(HTML_PREVIEW_DRAG_BEGIN_EVENT));
  },
  htmlPreviewDragMove(px) {
    window.dispatchEvent(new CustomEvent(HTML_PREVIEW_DRAG_MOVE_EVENT, { detail: px }));
  },
  htmlPreviewDragEnd(dismiss) {
    window.dispatchEvent(new CustomEvent(HTML_PREVIEW_DRAG_END_EVENT, { detail: dismiss }));
  },
  flushPersist() {
    flushPersist();
  },
};
