import { beforeEach, describe, expect, it, vi } from "vitest";

import * as bridge from "./bridge";
import { DeckShell } from "./shell";
import type { DeckCard } from "./state";

/// The gateway broadcasts `DeckCardData` / `DeckChanged` to whoever is
/// connected at that instant — no buffer, no replay — and native re-fetches
/// only on bridge-ready, on `DeckChanged`, and on a layout rollback. So a card
/// installed while this device's leg was down arrives at an already-warm shell
/// with no snapshot, and the shell is the only party that knows it is missing
/// one. These pin the two ways that used to end in a card showing placeholders
/// forever.

const card = (cardId: string): DeckCard => ({
  cardId,
  title: "Quota",
  position: 0,
  size: "wide",
  sizes: ["wide"],
  maximize: false,
  enabled: true,
  quarantined: false,
  specHash: "hash-1",
  lastSeq: 0,
});

function mount(): { shell: DeckShell; root: HTMLElement } {
  const root = document.createElement("div");
  document.body.append(root);
  return { shell: new DeckShell(root), root };
}

/// Drive the iframe's `load` handler the way the real WebView does.
function fireLoad(root: HTMLElement): HTMLIFrameElement | null {
  const frame = root.querySelector("iframe");
  frame?.dispatchEvent(new Event("load"));
  return frame;
}

describe("deck shell snapshot delivery", () => {
  beforeEach(() => {
    document.body.replaceChildren();
    vi.restoreAllMocks();
  });

  it("asks native to re-fetch when a mounted card has no snapshot", () => {
    const refetch = vi.spyOn(bridge, "postRefetch").mockImplementation(() => {});
    const { shell, root } = mount();

    // A card with NO snapshot — exactly what a refetch returns when the
    // install's DeckCardData was broadcast to nobody.
    shell.applyDeckState([card("c1")], []);
    shell.applyBundle("c1", "<div class=card><div id=x>–</div></div>");
    fireLoad(root);

    expect(refetch).toHaveBeenCalled();
  });

  it("asks only once per card, so a snapshot-less card cannot spin", () => {
    const refetch = vi.spyOn(bridge, "postRefetch").mockImplementation(() => {});
    const { shell, root } = mount();

    shell.applyDeckState([card("c1")], []);
    shell.applyBundle("c1", "<div class=card><div id=x>–</div></div>");
    fireLoad(root);
    shell.applyDeckState([card("c1")], []);
    shell.applyDeckState([card("c1")], []);

    expect(refetch).toHaveBeenCalledTimes(1);
  });

  it("does not ask when the snapshot is already there", () => {
    const refetch = vi.spyOn(bridge, "postRefetch").mockImplementation(() => {});
    const { shell, root } = mount();

    shell.applyDeckState(
      [card("c1")],
      [{ cardId: "c1", seq: 1, payload: '{"used":1}', error: null }],
    );
    shell.applyBundle("c1", "<div class=card><div id=x>–</div></div>");
    fireLoad(root);

    expect(refetch).not.toHaveBeenCalled();
  });

  /// The handover is the step that can fail, so it has to be the step that
  /// decides. Recording the port first left the shell posting every push into
  /// a channel the card never received — and `applyBundle` returns early once
  /// `tile.frame` is set, so nothing ever rebuilt it.
  it("drops the frame instead of keeping a port the card never received", () => {
    vi.spyOn(bridge, "postRefetch").mockImplementation(() => {});
    vi.spyOn(bridge, "log").mockImplementation(() => {});
    const { shell, root } = mount();

    shell.applyDeckState(
      [card("c1")],
      [{ cardId: "c1", seq: 1, payload: '{"used":1}', error: null }],
    );
    shell.applyBundle("c1", "<div class=card><div id=x>–</div></div>");

    const frame = root.querySelector("iframe");
    expect(frame).not.toBeNull();
    // jsdom gives a real contentWindow; take it away to model the frame that
    // loads without one.
    Object.defineProperty(frame!, "contentWindow", { value: null, configurable: true });
    frame!.dispatchEvent(new Event("load"));

    expect(root.querySelector("iframe"), "the unreachable frame must be dropped").toBeNull();
    expect(bridge.log).toHaveBeenCalledWith(
      "error",
      expect.stringContaining("card init failed"),
    );

    // And the card is re-mountable, rather than latched broken forever.
    shell.applyBundle("c1", "<div class=card><div id=x>–</div></div>");
    expect(root.querySelector("iframe")).not.toBeNull();
  });
});

/// iPhone Duo resizes the app LIVE between its outer (~466pt) and inner
/// (~951pt) displays, and Split View resizes it again. The drag engine pins the
/// lifted tile in viewport px captured at pickup, so a fold mid-press used to
/// leave the tile hanging offset and the release resolving its slot against a
/// grid that had reflowed under it.
describe("deck shell live resize", () => {
  const twoCards = () => [
    { ...card("a"), size: "small" as const, sizes: ["small" as const] },
    { ...card("b"), size: "small" as const, position: 1, sizes: ["small" as const] },
  ];

  let width = 466;

  beforeEach(() => {
    document.body.replaceChildren();
    vi.restoreAllMocks();
    vi.useFakeTimers();
    width = 466;
    vi.spyOn(window, "innerWidth", "get").mockImplementation(() => width);
    vi.spyOn(bridge, "postRefetch").mockImplementation(() => {});
    vi.spyOn(bridge, "postEditMode").mockImplementation(() => {});
    vi.spyOn(bridge, "postHaptic").mockImplementation(() => {});
    return () => vi.useRealTimers();
  });

  /// Long-press tile `a` in edit mode until the drag arms. jsdom has no
  /// layout, so every rect is zero and the release point (y > 0) resolves to
  /// the END of the deck — a reorder the drop path would persist.
  function liftA(): { shell: DeckShell; root: HTMLElement; tile: HTMLElement } {
    const { shell, root } = mount();
    shell.applyDeckState(twoCards(), []);
    shell.setEditMode(true);
    const tile = root.querySelector<HTMLElement>('[data-card-id="a"]')!;
    tile.dispatchEvent(
      new PointerEvent("pointerdown", { bubbles: true, clientX: 10, clientY: 10, pointerId: 1 }),
    );
    vi.advanceTimersByTime(400);
    expect(tile.classList.contains("deck-dragging-live"), "the long press must arm").toBe(true);
    return { shell, root, tile };
  }

  const resizeTo = (w: number) => {
    width = w;
    window.dispatchEvent(new Event("resize"));
  };

  it("drops at the release point when nothing resized (control)", () => {
    const layout = vi.spyOn(bridge, "postLayout").mockImplementation(() => {});
    const { tile } = liftA();

    window.dispatchEvent(new PointerEvent("pointerup", { pointerId: 1 }));
    vi.advanceTimersByTime(300);

    expect(tile.classList.contains("deck-dragging-live")).toBe(false);
    expect(layout).toHaveBeenCalledTimes(1);
  });

  it("aborts a drag in place when the viewport width changes", () => {
    const layout = vi.spyOn(bridge, "postLayout").mockImplementation(() => {});
    const { root, tile } = liftA();

    resizeTo(951);

    // Settled at once — no glide through stale geometry.
    expect(tile.classList.contains("deck-dragging-live")).toBe(false);
    expect(tile.style.left).toBe("");
    expect(tile.style.transform).toBe("");
    expect(root.querySelector(".deck-drop-slot")).toBeNull();
    expect(root.querySelector(".deck-grid")!.classList.contains("has-drag")).toBe(false);

    // The finger lifting later must not re-resolve a slot from the stale point.
    window.dispatchEvent(new PointerEvent("pointerup", { pointerId: 1 }));
    vi.advanceTimersByTime(300);
    expect(layout).not.toHaveBeenCalled();
    expect(tile.style.order).toBe("0");
  });

  it("keeps the drag through a height-only resize", () => {
    const { tile } = liftA();

    resizeTo(466);

    expect(tile.classList.contains("deck-dragging-live")).toBe(true);
  });
});

describe("deck shell maximize geometry", () => {
  beforeEach(() => {
    document.body.replaceChildren();
    vi.restoreAllMocks();
    vi.spyOn(bridge, "postRefetch").mockImplementation(() => {});
    vi.spyOn(bridge, "postMaximize").mockImplementation(() => {});
    vi.useFakeTimers();
    return () => vi.useRealTimers();
  });

  /// The expanded rect has to live in CSS (`.deck-max-full`): inline px can't
  /// follow a live fold/unfold, and only CSS can clear each safe-area inset on
  /// its own side — the sandboxed card can't see the shell's insets.
  it("hands the expanded rect to CSS instead of pinning viewport px", () => {
    const { shell, root } = mount();
    shell.applyDeckState([{ ...card("c1"), maximize: true }], []);
    const tile = root.querySelector<HTMLElement>('[data-card-id="c1"]')!;

    tile.querySelector<HTMLButtonElement>(".deck-max-btn")!.click();

    expect(tile.classList.contains("maximized")).toBe(true);
    expect(tile.classList.contains("deck-max-full")).toBe(true);
    expect(tile.style.left).toBe("");
    expect(tile.style.width).toBe("");

    // Restore is inert until the expand settles (the transitionend backstop).
    vi.advanceTimersByTime(400);
    shell.restoreMaximizedCard();
    expect(tile.classList.contains("deck-max-full")).toBe(false);
    expect(tile.style.width).not.toBe("");
  });
});
