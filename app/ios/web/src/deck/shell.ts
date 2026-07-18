// The deck shell: a 2-column size-class grid of card tiles, each an
// <iframe sandbox="allow-scripts" srcdoc=…> in an opaque origin with an
// injected CSP. Card identity is the per-card MessagePort transferred at
// init — ports are held in a shell-side map the card can neither see nor
// forge; closing the port mutes a removed card instantly.

import Sortable from "sortablejs";
import sdkSource from "./sdkCard.js?raw";
import * as bridge from "./bridge";
import { en } from "../locales/en";
import { zh } from "../locales/zh";
import {
  DeckCard,
  DeckState,
  EMPTY_STATE,
  applyCardData,
  buildSrcdoc,
  cycleSize,
  layoutEntries,
  parsePayload,
  reorderTo,
  replaceState,
  setCardSize,
} from "./state";

// Locale DATA only — the i18next runtime lives in the React transcript entry
// and must not be dragged into this vanilla chunk for four strings.
const STRINGS: Record<string, Record<string, string>> = {
  en: en.translation.deck,
  zh: zh.translation.deck,
};

/// Long-press threshold entering edit mode (matches the iOS wiggle idiom).
const EDIT_HOLD_MS = 450;

type Tile = {
  card: DeckCard;
  el: HTMLElement;
  frame: HTMLIFrameElement | null;
  port: MessagePort | null;
  bundleRequested: boolean;
  overlay: HTMLElement;
  sizeBtn: HTMLButtonElement;
  removeBtn: HTMLButtonElement;
};

export class DeckShell {
  private root: HTMLElement;
  private grid: HTMLElement;
  private emptyEl: HTMLElement;
  private state: DeckState = EMPTY_STATE;
  private tiles = new Map<string, Tile>();
  private pendingCalls = new Map<string, { cardId: string; localId: number }>();
  private nextCallSerial = 1;
  private editMode = false;
  private lang = "en";
  private sortable: Sortable;

  constructor(root: HTMLElement) {
    this.root = root;
    this.grid = document.createElement("div");
    this.grid.className = "deck-grid";
    this.emptyEl = document.createElement("div");
    this.emptyEl.className = "deck-empty";
    this.root.append(this.grid, this.emptyEl);
    // SortableJS owns the drag while editing: the fallback clone follows
    // the finger, displaced neighbors FLIP into place, and the settled
    // DOM order becomes the state. forceFallback because WKWebView's
    // native HTML5 DnD is unreliable and the fallback styles identically
    // on every surface. The touch delay lets a quick flick still scroll
    // the board in edit mode.
    this.sortable = Sortable.create(this.grid, {
      disabled: true,
      animation: 250,
      easing: "cubic-bezier(0.2, 0, 0, 1)",
      draggable: ".deck-card",
      filter: ".deck-size-btn, .deck-remove-btn",
      preventOnFilter: false,
      forceFallback: true,
      fallbackClass: "deck-drag-fallback",
      fallbackTolerance: 4,
      ghostClass: "deck-drag-ghost",
      delay: 100,
      delayOnTouchOnly: true,
      touchStartThreshold: 3,
      onEnd: (evt) => {
        if (evt.oldIndex === evt.newIndex) return;
        const ids = [...this.grid.children].map(
          (el) => (el as HTMLElement).dataset.cardId ?? "",
        );
        this.state = { ...this.state, cards: reorderTo(this.state.cards, ids) };
        bridge.postLayout(layoutEntries(this.state.cards));
      },
    });
    this.renderEmpty();
  }

  private t(key: string): string {
    const table = STRINGS[this.lang] ?? STRINGS.en;
    return table[key] ?? STRINGS.en[key] ?? key;
  }

  setLanguage(lang: string): void {
    this.lang = lang.startsWith("zh") ? "zh" : "en";
    this.renderEmpty();
    for (const tile of this.tiles.values()) this.renderOverlay(tile);
  }

  setEditMode(active: boolean): void {
    if (this.editMode === active) return;
    this.editMode = active;
    this.grid.classList.toggle("editing", active);
    this.sortable.option("disabled", !active);
    bridge.postEditMode(active);
    for (const tile of this.tiles.values()) this.renderOverlay(tile);
  }

  /// REPLACE from init / refetch: cached snapshots + seqs are replaced
  /// unconditionally; tiles diff against the new card list.
  applyDeckState(cards: DeckCard[], snapshots: Parameters<typeof replaceState>[1]): void {
    this.state = replaceState(cards, snapshots);
    this.reconcileTiles();
    for (const card of this.state.cards) this.pushSnapshot(card.cardId);
    this.renderEmpty();
  }

  applyCardData(cardId: string, seq: number, payload: string): void {
    const { state, changed } = applyCardData(this.state, cardId, seq, payload);
    this.state = state;
    if (changed) this.pushSnapshot(cardId);
  }

  applyBundle(cardId: string, cardHtml: string): void {
    const tile = this.tiles.get(cardId);
    if (!tile || tile.frame) return;
    const frame = document.createElement("iframe");
    frame.className = "deck-card-frame";
    frame.setAttribute("sandbox", "allow-scripts");
    frame.srcdoc = buildSrcdoc(cardHtml, sdkSource);
    frame.addEventListener("load", () => {
      const channel = new MessageChannel();
      tile.port = channel.port1;
      channel.port1.onmessage = (e) => this.onCardMessage(cardId, e.data);
      frame.contentWindow?.postMessage(
        { type: "deck_init", size: tile.card.size },
        "*",
        [channel.port2],
      );
      this.pushSnapshot(cardId);
    });
    tile.frame = frame;
    tile.el.querySelector(".deck-card-body")?.append(frame);
  }

  applyCallResult(id: string, ok: boolean, value: unknown, error?: string): void {
    const pending = this.pendingCalls.get(id);
    if (!pending) return;
    this.pendingCalls.delete(id);
    const tile = this.tiles.get(pending.cardId);
    tile?.port?.postMessage({
      type: "call_result",
      id: pending.localId,
      ok,
      value,
      error,
    });
  }

  // ---- internals ----------------------------------------------------

  private onCardMessage(cardId: string, msg: unknown): void {
    const m = (msg ?? {}) as {
      type?: string;
      id?: number;
      op?: string;
      params?: unknown;
      level?: string;
      message?: string;
    };
    if (m.type === "call" && typeof m.id === "number" && typeof m.op === "string") {
      const globalId = `${cardId}#${this.nextCallSerial++}`;
      this.pendingCalls.set(globalId, { cardId, localId: m.id });
      bridge.postCall(globalId, cardId, m.op, m.params ?? {});
    } else if (m.type === "log") {
      bridge.log(m.level === "error" ? "error" : "info", `[card ${cardId}] ${m.message ?? ""}`);
    }
    // Anything else from a card is ignored: port identity is the card's
    // only capability, and the surface is exactly call + log.
  }

  private pushSnapshot(cardId: string): void {
    const tile = this.tiles.get(cardId);
    const cell = this.state.snaps[cardId];
    if (!tile || !tile.port || !cell) return;
    tile.port.postMessage({ type: "data", payload: parsePayload(cell) });
  }

  private renderEmpty(): void {
    this.emptyEl.textContent = this.t("empty");
    this.emptyEl.style.display = this.state.cards.length === 0 ? "" : "none";
  }

  private reconcileTiles(): void {
    const seen = new Set<string>();
    for (const card of this.state.cards) {
      seen.add(card.cardId);
      let tile = this.tiles.get(card.cardId);
      if (!tile) {
        tile = this.createTile(card);
        this.tiles.set(card.cardId, tile);
      }
      const specChanged = tile.card.specHash !== card.specHash;
      tile.card = card;
      tile.el.dataset.size = card.size;
      if (specChanged && tile.frame) {
        // New code: tear the old iframe down and refetch the bundle.
        tile.port?.close();
        tile.port = null;
        tile.frame.remove();
        tile.frame = null;
        tile.bundleRequested = false;
      }
      if (!tile.bundleRequested) {
        tile.bundleRequested = true;
        bridge.postRequestBundle(card.cardId);
      }
      tile.port?.postMessage({ type: "size", size: card.size });
      this.renderOverlay(tile);
      this.grid.append(tile.el); // append order == flow order
    }
    for (const [cardId, tile] of this.tiles) {
      if (!seen.has(cardId)) {
        tile.port?.close();
        tile.el.remove();
        this.tiles.delete(cardId);
      }
    }
  }

  private createTile(card: DeckCard): Tile {
    const el = document.createElement("div");
    el.className = "deck-card";
    el.dataset.cardId = card.cardId;
    el.dataset.size = card.size;

    const header = document.createElement("div");
    header.className = "deck-card-header";
    const title = document.createElement("span");
    title.className = "deck-card-title";
    title.textContent = card.title;
    header.append(title);

    const body = document.createElement("div");
    body.className = "deck-card-body";

    const overlay = document.createElement("div");
    overlay.className = "deck-card-overlay";

    const sizeBtn = document.createElement("button");
    sizeBtn.className = "deck-size-btn";
    sizeBtn.textContent = "⤢";
    sizeBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      if (!this.editMode) return;
      const next = cycleSize(this.tiles.get(card.cardId)?.card.size ?? card.size);
      this.state = { ...this.state, cards: setCardSize(this.state.cards, card.cardId, next) };
      this.reconcileTiles();
      bridge.postLayout(layoutEntries(this.state.cards));
    });

    const removeBtn = document.createElement("button");
    removeBtn.className = "deck-remove-btn";
    removeBtn.textContent = "✕";
    removeBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      if (!this.editMode) return;
      bridge.postCardAction(card.cardId, "delete");
    });

    el.append(header, body, overlay, sizeBtn, removeBtn);
    this.wireGestures(el);
    return { card, el, frame: null, port: null, bundleRequested: false, overlay, sizeBtn, removeBtn };
  }

  private renderOverlay(tile: Tile): void {
    const { card, overlay } = tile;
    overlay.replaceChildren();
    const showFace = card.quarantined || !card.enabled;
    overlay.style.display = showFace ? "" : "none";
    tile.el.classList.toggle("faulted", card.quarantined);
    if (!showFace) return;
    const line = document.createElement("div");
    line.className = "deck-face-line";
    line.textContent = card.quarantined ? this.t("quarantined") : this.t("disabled");
    const btn = document.createElement("button");
    btn.className = "deck-face-btn";
    btn.textContent = this.t("reenable");
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      bridge.postCardAction(card.cardId, "enable");
    });
    overlay.append(line, btn);
  }

  /// Long-press enters edit mode; the drag itself belongs to SortableJS
  /// (enabled in `setEditMode`), so this only arms the hold-to-edit
  /// timer and cancels it when the touch turns into a scroll.
  private wireGestures(el: HTMLElement): void {
    let holdTimer: ReturnType<typeof setTimeout> | null = null;

    el.addEventListener("pointerdown", (down) => {
      if (this.editMode) return;
      holdTimer = setTimeout(() => this.setEditMode(true), EDIT_HOLD_MS);
      const clearHold = () => {
        if (holdTimer) {
          clearTimeout(holdTimer);
          holdTimer = null;
        }
      };
      const move = (ev: PointerEvent) => {
        if (
          Math.abs(ev.clientX - down.clientX) > 8 ||
          Math.abs(ev.clientY - down.clientY) > 8
        ) {
          clearHold();
        }
      };
      const up = () => {
        window.removeEventListener("pointermove", move);
        window.removeEventListener("pointerup", up);
        window.removeEventListener("pointercancel", up);
        clearHold();
      };
      window.addEventListener("pointermove", move);
      window.addEventListener("pointerup", up);
      window.addEventListener("pointercancel", up);
    });
  }
}
