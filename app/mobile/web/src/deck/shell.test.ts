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
