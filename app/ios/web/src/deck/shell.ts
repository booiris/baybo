// The deck shell: a 2-column size-class grid of card tiles, each an
// <iframe sandbox="allow-scripts" srcdoc=…> in an opaque origin with an
// injected CSP. Card identity is the per-card MessagePort transferred at
// init — ports are held in a shell-side map the card can neither see nor
// forge; closing the port mutes a removed card instantly.

import sdkSource from "./sdkCard.js?raw";
import cardBaseCss from "./cardBase.css?raw";
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
// `Partial` because `t()` looks keys up dynamically — a miss is real.
const STRINGS: Record<"en" | "zh", Partial<Record<string, string>>> = {
  en: en.translation.deck,
  zh: zh.translation.deck,
};

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
  private setupInflight = false;
  private lang: "en" | "zh" = "en";
  // Live drag state (the iOS-home-screen engine below).
  private dragCardId: string | null = null;
  private dragEl: HTMLElement | null = null;
  private placeholder: HTMLElement | null = null;
  private placeholderIndex = 0;
  private dragOrigin = { x: 0, y: 0, left: 0, top: 0 };
  private lastPoint = { x: 0, y: 0 };
  private dragRaf = 0;

  constructor(root: HTMLElement) {
    this.root = root;
    this.grid = document.createElement("div");
    this.grid.className = "deck-grid";
    this.emptyEl = document.createElement("div");
    this.emptyEl.className = "deck-empty";
    this.root.append(this.grid, this.emptyEl);
    this.renderEmpty();
  }

  private t(key: string): string {
    return STRINGS[this.lang][key] ?? STRINGS.en[key] ?? key;
  }

  setLanguage(lang: string): void {
    this.lang = lang.startsWith("zh") ? "zh" : "en";
    this.renderEmpty();
    for (const tile of this.tiles.values()) this.renderOverlay(tile);
  }

  /// A `/deck` creation from the empty-board CTA is running (no card yet):
  /// the CTA shows a spinner and a re-tap returns to that chat (native
  /// routes it; the shell just reflects the state).
  setSetupInflight(active: boolean): void {
    this.setupInflight = active;
    this.renderEmpty();
  }

  setEditMode(active: boolean): void {
    if (this.editMode === active) return;
    this.editMode = active;
    if (!active && this.dragCardId !== null) this.endDrag();
    this.grid.classList.toggle("editing", active);
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
    frame.srcdoc = buildSrcdoc(cardHtml, sdkSource, cardBaseCss);
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
    if (!tile || !tile.port || cell === undefined) return;
    tile.port.postMessage({ type: "data", payload: parsePayload(cell) });
  }

  private renderEmpty(): void {
    const empty = this.state.cards.length === 0;
    this.emptyEl.style.display = empty ? "" : "none";
    if (!empty) return;
    // Rebuilt each call (also on setLanguage), so the copy + CTA follow
    // the active locale.
    this.emptyEl.replaceChildren();
    const msg = document.createElement("div");
    msg.textContent = this.t("empty");
    const cta = document.createElement("button");
    cta.className = "deck-empty-cta";
    // Same CTA either way — native decides new-vs-return from its own
    // setup-session state; the shell only shows which mode it's in.
    if (this.setupInflight) {
      cta.classList.add("inflight");
      const spin = document.createElement("span");
      spin.className = "deck-cta-spin";
      cta.append(spin, document.createTextNode(this.t("createCardInflight")));
    } else {
      cta.textContent = this.t("quickSetup");
    }
    cta.addEventListener("click", () =>
      bridge.postQuickSetup(this.t("quickSetupPrompt")),
    );
    this.emptyEl.append(msg, cta);
  }

  private reconcileTiles(): void {
    const seen = new Set<string>();
    for (const card of this.state.cards) {
      seen.add(card.cardId);
      let tile = this.tiles.get(card.cardId);
      if (!tile) {
        tile = this.createTile(card);
        this.tiles.set(card.cardId, tile);
        this.grid.append(tile.el);
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
    }
    for (const [cardId, tile] of this.tiles) {
      if (!seen.has(cardId)) {
        tile.port?.close();
        tile.el.remove();
        this.tiles.delete(cardId);
      }
    }
    // Placement is CSS `order`, NEVER a DOM move: relocating an <iframe>
    // in the tree reloads its document (the historical "whole page
    // flashes" bug). Tiles are appended once at creation and stay put;
    // reorders are pure style, so no reconcile — and no drag settle —
    // ever reboots a card.
    let i = 0;
    for (const card of this.state.cards) {
      const el = this.tiles.get(card.cardId)?.el;
      if (el) el.style.order = String(i++);
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
    this.wireDrag(el, card.cardId);
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

  // ---- drag engine ---------------------------------------------------
  //
  // iOS-home-screen mechanics, built around one constraint: an <iframe>
  // reloads whenever its node MOVES in the DOM, but transform and CSS
  // `order` never touch the tree. So the REAL tile follows the finger
  // (fixed + translate — its document keeps painting live), a plain-div
  // placeholder holds the target slot via `order`, neighbors FLIP-slide,
  // and the drop is pure style. Zero reloads at any point. In edit mode
  // the drag arms on the first few px of movement — no hold delay,
  // matching the wiggle-mode feel.

  private wireDrag(el: HTMLElement, cardId: string): void {
    // The one thing that reliably stops WKWebView's native UIScrollView
    // pan from claiming the touch (it fires pointercancel ~2 moves in,
    // killing the drag — `touch-action: none` alone did NOT hold once
    // beginDrag's position:fixed flip triggered gesture re-arbitration).
    // touchMOVE only, so taps still synthesize clicks for the buttons.
    el.addEventListener(
      "touchmove",
      (e) => {
        if (this.editMode) e.preventDefault();
      },
      { passive: false },
    );
    el.addEventListener("pointerdown", (down) => {
      if (!this.editMode || this.dragCardId !== null) return;
      const target = down.target as HTMLElement;
      if (target.closest(".deck-size-btn, .deck-remove-btn, .deck-face-btn")) return;
      const startX = down.clientX;
      const startY = down.clientY;
      // Capture the pointer: beginDrag flips the tile to position:fixed
      // mid-gesture, and without capture WebKit re-hit-tests and drops
      // the move stream right there (observed: drags died ~16px in).
      try {
        el.setPointerCapture(down.pointerId);
      } catch {
        // Capture is best-effort; an already-released pointer just
        // degrades to the uncaptured behavior.
      }
      let started = false;
      const move = (ev: PointerEvent) => {
        if (ev.pointerId !== down.pointerId) return;
        if (!started) {
          if (Math.abs(ev.clientX - startX) < 4 && Math.abs(ev.clientY - startY) < 4) {
            return;
          }
          started = true;
          this.beginDrag(el, cardId, startX, startY);
        }
        this.lastPoint = { x: ev.clientX, y: ev.clientY };
        if (this.dragRaf === 0) {
          this.dragRaf = requestAnimationFrame(() => {
            this.dragRaf = 0;
            this.dragFrame();
          });
        }
      };
      const up = () => {
        window.removeEventListener("pointermove", move);
        window.removeEventListener("pointerup", up);
        window.removeEventListener("pointercancel", up);
        if (started) this.endDrag();
      };
      window.addEventListener("pointermove", move);
      window.addEventListener("pointerup", up);
      window.addEventListener("pointercancel", up);
    });
  }

  private beginDrag(el: HTMLElement, cardId: string, grabX: number, grabY: number): void {
    const rect = el.getBoundingClientRect();
    this.dragCardId = cardId;
    this.dragEl = el;
    this.dragOrigin = { x: grabX, y: grabY, left: rect.left, top: rect.top };
    this.lastPoint = { x: grabX, y: grabY };
    this.placeholderIndex = this.state.cards.findIndex((c) => c.cardId === cardId);

    const ph = document.createElement("div");
    ph.className = "deck-drop-slot";
    ph.dataset.size = el.dataset.size ?? "wide";
    this.placeholder = ph;

    this.flipTiles(() => {
      this.grid.append(ph);
      // Lift the tile out of flow at its exact on-screen rect; from here
      // it is positioned by transform alone.
      el.classList.add("deck-dragging-live");
      el.style.left = `${rect.left}px`;
      el.style.top = `${rect.top}px`;
      el.style.width = `${rect.width}px`;
      el.style.height = `${rect.height}px`;
      this.applyPlaceholderOrder();
    });
    this.grid.classList.add("has-drag");
  }

  private dragFrame(): void {
    const el = this.dragEl;
    if (!el || this.dragCardId === null) return;
    const dx = this.lastPoint.x - this.dragOrigin.x;
    const dy = this.lastPoint.y - this.dragOrigin.y;
    el.style.transform = `translate(${dx}px, ${dy}px) scale(1.04)`;
    const target = this.dropIndexAt(this.lastPoint.x, this.lastPoint.y);
    if (target !== this.placeholderIndex) {
      this.placeholderIndex = target;
      this.flipTiles(() => this.applyPlaceholderOrder());
    }
  }

  private endDrag(): void {
    const el = this.dragEl;
    const ph = this.placeholder;
    const cardId = this.dragCardId;
    if (!el || !ph || cardId === null) return;
    // The last pointer movement may still be waiting on rAF — resolve
    // the final slot from the actual drop point, not the last frame.
    if (this.dragRaf !== 0) {
      cancelAnimationFrame(this.dragRaf);
      this.dragRaf = 0;
    }
    const finalTarget = this.dropIndexAt(this.lastPoint.x, this.lastPoint.y);
    if (finalTarget !== this.placeholderIndex) {
      this.placeholderIndex = finalTarget;
      this.flipTiles(() => this.applyPlaceholderOrder());
    }
    this.dragCardId = null;

    // Ease the lifted tile onto the slot, then settle to pure style: the
    // tile rejoins the flow via `order` — no DOM move, no reload.
    const phRect = ph.getBoundingClientRect();
    const dx = phRect.left - this.dragOrigin.left;
    const dy = phRect.top - this.dragOrigin.top;
    el.style.transition = "transform 180ms cubic-bezier(0.2, 0, 0, 1)";
    el.style.transform = `translate(${dx}px, ${dy}px)`;

    const others = this.state.cards
      .filter((c) => c.cardId !== cardId)
      .map((c) => c.cardId);
    others.splice(this.placeholderIndex, 0, cardId);

    let settled = false;
    const settle = () => {
      if (settled) return;
      settled = true;
      el.removeEventListener("transitionend", settle);
      el.classList.remove("deck-dragging-live");
      el.style.cssText = "";
      ph.remove();
      this.placeholder = null;
      this.dragEl = null;
      this.grid.classList.remove("has-drag");
      const next = reorderTo(this.state.cards, others);
      const changed = next.some(
        (c, i) => c.cardId !== this.state.cards[i]?.cardId,
      );
      this.state = { ...this.state, cards: next };
      this.reconcileTiles();
      if (changed) bridge.postLayout(layoutEntries(this.state.cards));
    };
    el.addEventListener("transitionend", settle);
    // Transitionend can be swallowed (display churn, zero-distance move);
    // the timer is the backstop so a drop can never wedge mid-settle.
    setTimeout(settle, 240);
  }

  /// Others keep their relative order (0,1,2,… skipping the dragged
  /// tile); indices at/after the slot shift +1 and the placeholder takes
  /// the slot value, so CSS grid renders the insertion preview.
  private applyPlaceholderOrder(): void {
    let i = 0;
    for (const card of this.state.cards) {
      if (card.cardId === this.dragCardId) continue;
      const el = this.tiles.get(card.cardId)?.el;
      if (!el) continue;
      el.style.order = String(i >= this.placeholderIndex ? i + 1 : i);
      i += 1;
    }
    this.placeholder?.style.setProperty("order", String(this.placeholderIndex));
  }

  /// Insertion index for the pointer, judged against the LIVE rects of
  /// the other tiles (they move as the placeholder does). Axis-aware:
  /// a full-width tile swaps on the VERTICAL midline (dragging past its
  /// center is enough — no deep dragging), while a narrow (half-width)
  /// tile keeps the left/right test for side-by-side pairs.
  private dropIndexAt(x: number, y: number): number {
    const gridWidth = this.grid.clientWidth;
    let index = 0;
    for (const card of this.state.cards) {
      if (card.cardId === this.dragCardId) continue;
      const el = this.tiles.get(card.cardId)?.el;
      if (!el) continue;
      const r = el.getBoundingClientRect();
      const narrow = r.width < gridWidth * 0.6;
      const after = narrow
        ? y > r.bottom || (y >= r.top && x > r.left + r.width / 2)
        : y > r.top + r.height / 2;
      if (after) index += 1;
    }
    return index;
  }

  /// FLIP everything except the lifted tile across `mutate`: capture
  /// rects, mutate, animate the delta back to zero. WAAPI so the wiggle
  /// keyframes (paused during a drag via `.has-drag`) can't fight the
  /// slide on the `transform` channel.
  private flipTiles(mutate: () => void): void {
    const before: Array<{ el: HTMLElement; left: number; top: number }> = [];
    for (const tile of this.tiles.values()) {
      if (tile.el === this.dragEl) continue;
      const r = tile.el.getBoundingClientRect();
      before.push({ el: tile.el, left: r.left, top: r.top });
    }
    mutate();
    for (const { el, left, top } of before) {
      const n = el.getBoundingClientRect();
      const dx = left - n.left;
      const dy = top - n.top;
      if (dx !== 0 || dy !== 0) {
        el.animate(
          [{ transform: `translate(${dx}px, ${dy}px)` }, { transform: "none" }],
          { duration: 220, easing: "cubic-bezier(0.2, 0, 0, 1)" },
        );
      }
    }
  }
}
