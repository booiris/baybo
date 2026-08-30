// Deck shell ⇄ native bridge, mirroring the transcript bridge's shape:
// native drives `window.deckShell.*`; the shell posts `{type: …}` bodies
// on the `deck` WKScriptMessageHandler. In a plain dev browser posts
// degrade to console.log stubs.

import type { DeckCard, DeckSnapshot, LayoutEntry } from "./state";

export type DeckStatePayload = {
  cards: DeckCard[];
  snapshots: DeckSnapshot[];
};

export type DeckInitPayload = DeckStatePayload & {
  lang: string;
  editMode?: boolean;
};

export type CardDataPayload = { cardId: string; seq: number; payload: string };
export type BundlePayload = { cardId: string; cardHtml: string };
export type CallResultPayload = {
  id: string;
  ok: boolean;
  value?: unknown;
  error?: string;
};

export type PickResultPayload = {
  id: string;
  ok: boolean;
  ref?: unknown;
  error?: string;
};

export type DeckShellGlobal = {
  init(payload: DeckInitPayload): void;
  deckState(payload: DeckStatePayload): void;
  cardData(payload: CardDataPayload): void;
  bundle(payload: BundlePayload): void;
  callResult(payload: CallResultPayload): void;
  /// Native → web: a `deck.pickBlob` request resolved (a blob ref) or failed
  /// (busy / cancelled / upload error).
  pickResult(payload: PickResultPayload): void;
  setEditMode(active: boolean): void;
  setLanguage(lang: string): void;
  /// Native → web: the header's ✕ was tapped; collapse the maximized card.
  restoreMaximized(): void;
};

export type CardAction = "enable" | "disable" | "delete";

// `Window.webkit` is declared once, in src/bridge.ts (a global interface
// member may not be re-declared with a different shape); the `deck`
// message handler is part of that declaration.
declare global {
  interface Window {
    deckShell: DeckShellGlobal;
  }
}

const native = window.webkit?.messageHandlers?.deck;

function post(message: Record<string, unknown>): void {
  if (native) native.postMessage(message);
  else console.log("[deck bridge]", message);
}

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

/// Ask native to refetch GET /v1/deck (answered via deckShell.deckState).
export function postRefetch(): void {
  post({ type: "refetch" });
}

export function postRequestBundle(cardId: string): void {
  post({ type: "requestBundle", cardId });
}

export function postCall(
  id: string,
  cardId: string,
  op: string,
  params: unknown,
): void {
  post({ type: "call", id, cardId, op, params });
}

export function postLayout(entries: LayoutEntry[]): void {
  post({ type: "layout", entries });
}

/// A card asked to pick a blob (`deck.pickBlob`): native presents a picker,
/// uploads the choice, and answers via `deckShell.pickResult`. `accept` is the
/// shell-normalized mime-glob list (`normalizeAccept`), or null for "no
/// constraint" — native elects the photo library or the file browser from it.
export function postPick(id: string, cardId: string, accept: string | null): void {
  post({ type: "pick", id, cardId, accept });
}

/// A card asked to share a blob (`deck.shareBlob`): native fetches the bytes
/// and presents the system share sheet. Fire-and-forget (no reply).
export function postShare(
  blobId: string,
  filename: string | null,
  contentType: string | null,
): void {
  post({ type: "share", blobId, filename, contentType });
}

/// Destructive actions (delete) are CONFIRMED natively — the shell only
/// reports intent; native shows its ConfirmDialog before calling the FFI.
export function postCardAction(cardId: string, action: CardAction): void {
  post({ type: "cardAction", cardId, action });
}

export function postEditMode(active: boolean): void {
  post({ type: "editMode", active });
}

/// A card entered/left its full-screen maximized layout. Native reflects it
/// by fading the wordmark header out (the tab bar stays); the shell owns the
/// tile's own expand/collapse animation.
export function postMaximize(active: boolean): void {
  post({ type: "maximize", active });
}

/// Fire a native impact haptic (the long-press reorder pickup).
export function postHaptic(): void {
  postSafe({ type: "haptic" });
}
